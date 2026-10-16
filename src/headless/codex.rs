//! A client for Codex's app-server (`codex app-server`, JSON-RPC over stdio).
//!
//! The agent's BBS tools are declared as dynamic tools on the thread and
//! answered in this process, so no MCP server is involved. Turns run with no
//! approvals and no sandbox, matching how ACP harnesses run. Turns can be
//! steered: mail is added to the running turn with `turn/steer`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::harness::{Event, MAX_INPUT_CHARS, SETUP_TIMEOUT, truncate};
use super::mcp::{Tools, tool_list};
use super::rpc::{Incoming, Process, Rpc};
use super::settings::HarnessConfig;

/// The running turn, if any.
#[derive(Default)]
struct Turn {
    /// Known once `turn/started` or the `turn/start` response arrives.
    id: Option<String>,
    /// Resolved by `turn/completed`.
    done: Option<oneshot::Sender<Result<String>>>,
}

/// One running app-server and the thread it serves.
#[derive(Clone)]
pub struct Codex {
    rpc: Rpc,
    thread_id: String,
    pub resumed: bool,
    turn: Arc<Mutex<Turn>>,
}

impl Codex {
    /// Start the app-server and open a thread: resume `resume` when it can,
    /// else start a new one with the BBS tools.
    pub async fn start(
        config: &HarnessConfig,
        cwd: &Path,
        stderr: &Path,
        tools: Tools,
        resume: Option<&str>,
    ) -> Result<(Codex, Process, mpsc::UnboundedReceiver<Event>)> {
        let (events, rx) = mpsc::unbounded_channel();
        let turn = Arc::new(Mutex::new(Turn::default()));
        let handler = {
            let turn = turn.clone();
            let tools = Arc::new(tokio::sync::Mutex::new(tools));
            move |rpc: &Rpc, msg: Incoming| handle(rpc, msg, &turn, &tools, &events)
        };
        let (rpc, mut process) = Rpc::spawn(&config.command, &config.env, cwd, stderr, handler)?;
        let mut codex = Codex {
            rpc,
            thread_id: String::new(),
            resumed: false,
            turn,
        };
        match timeout(SETUP_TIMEOUT, codex.open_thread(config, cwd, resume)).await {
            Ok(Ok(())) => Ok((codex, process, rx)),
            Ok(Err(err)) => {
                process.stop().await;
                Err(err)
            }
            Err(_) => {
                process.stop().await;
                bail!("the harness did not open a thread within {SETUP_TIMEOUT:?}")
            }
        }
    }

    async fn open_thread(
        &mut self,
        config: &HarnessConfig,
        cwd: &Path,
        resume: Option<&str>,
    ) -> Result<()> {
        self.rpc
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "bbs", "version": env!("CARGO_PKG_VERSION") },
                    // Dynamic tools are an experimental field of thread/start.
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        // A resumed thread keeps its tools; sending these again applies changes.
        let mut params = json!({
            "cwd": cwd.display().to_string(),
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        });
        if let Some(model) = &config.model {
            params["model"] = model.clone().into();
        }
        if let Some(provider) = &config.provider {
            params["modelProvider"] = provider.clone().into();
        }
        if let Some(id) = resume {
            let mut resume_params = params.clone();
            resume_params["threadId"] = id.into();
            match self.rpc.request("thread/resume", resume_params).await {
                Ok(_) => {
                    self.thread_id = id.to_string();
                    self.resumed = true;
                    return Ok(());
                }
                Err(err) => tracing::warn!("resuming codex thread {id} failed: {err:#}"),
            }
        }
        params["dynamicTools"] = dynamic_tools();
        let result = self.rpc.request("thread/start", params).await?;
        self.thread_id = result["thread"]["id"]
            .as_str()
            .context("thread/start returned no thread id")?
            .to_string();
        Ok(())
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Run one turn; returns its status (`completed` or `interrupted`).
    pub async fn prompt(&self, text: &str) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.turn.lock().unwrap().done = Some(tx);
        let params = json!({ "threadId": self.thread_id, "input": input(text) });
        let result = match self.rpc.request("turn/start", params).await {
            Ok(result) => result,
            Err(err) => {
                *self.turn.lock().unwrap() = Turn::default();
                return Err(err);
            }
        };
        {
            let mut turn = self.turn.lock().unwrap();
            if turn.done.is_some() && turn.id.is_none() {
                turn.id = result["turn"]["id"].as_str().map(str::to_string);
            }
        }
        rx.await.map_err(|_| anyhow!("the harness exited"))?
    }

    /// Add `text` to the running turn. Fails when no turn is running or the
    /// turn cannot take input (it is ending, say).
    pub async fn steer(&self, text: &str) -> Result<()> {
        let id = self.turn.lock().unwrap().id.clone();
        let id = id.context("no turn is running")?;
        self.rpc
            .request(
                "turn/steer",
                json!({ "threadId": self.thread_id, "expectedTurnId": id, "input": input(text) }),
            )
            .await?;
        Ok(())
    }

    /// Interrupt the running turn; its prompt then returns `interrupted`.
    pub async fn cancel(&self) -> Result<()> {
        let id = self.turn.lock().unwrap().id.clone();
        if let Some(id) = id {
            self.rpc
                .request(
                    "turn/interrupt",
                    json!({ "threadId": self.thread_id, "turnId": id }),
                )
                .await?;
        }
        Ok(())
    }
}

fn input(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// The BBS tools as plain functions, which reach the model without a namespace.
fn dynamic_tools() -> Value {
    let mut tools = tool_list();
    for tool in tools.as_array_mut().expect("tool list is an array") {
        tool["type"] = "function".into();
    }
    tools
}

fn handle(
    rpc: &Rpc,
    msg: Incoming,
    turn: &Mutex<Turn>,
    tools: &Arc<tokio::sync::Mutex<Tools>>,
    events: &mpsc::UnboundedSender<Event>,
) {
    match msg {
        Incoming::Request { id, method, params } if method == "item/tool/call" => {
            let rpc = rpc.clone();
            let tools = tools.clone();
            tokio::spawn(async move {
                let name = params["tool"].as_str().unwrap_or_default();
                // Tool failures are results the agent can read.
                let (text, success) =
                    match tools.lock().await.call(name, &params["arguments"]).await {
                        Ok(text) => (text, true),
                        Err(err) => (format!("{err:#}"), false),
                    };
                let reply = json!({
                    "contentItems": [{ "type": "inputText", "text": text }],
                    "success": success,
                });
                let _ = rpc.respond(id, Ok(reply)).await;
            });
        }
        Incoming::Request { id, method, params } => {
            rpc.respond_later(id, answer(&method, &params, events));
        }
        Incoming::Notification { method, params } => {
            if let Some(event) = notification_event(&method, &params, turn) {
                let _ = events.send(event);
            }
        }
        Incoming::Exited => {
            // Dropping the sender fails the running prompt.
            *turn.lock().unwrap() = Turn::default();
            let _ = events.send(Event::Exited);
        }
    }
}

/// Approve the approval requests that the `never` policy should already
/// prevent; refuse anything else.
fn answer(
    method: &str,
    params: &Value,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<Value, String> {
    let decision = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => "accept",
        "execCommandApproval" | "applyPatchApproval" => "approved",
        _ => return Err(format!("bbs headless does not support {method}")),
    };
    let _ = events.send(Event::Permission(json!({
        "title": params["command"].as_str().unwrap_or(method),
        "option": decision,
    })));
    Ok(json!({ "decision": decision }))
}

/// The event a notification reports, tracking the running turn as it goes.
fn notification_event(method: &str, params: &Value, turn: &Mutex<Turn>) -> Option<Event> {
    let delta = || params["delta"].as_str().map(str::to_string);
    Some(match method {
        "item/agentMessage/delta" => Event::Text(delta()?),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => Event::Thought(delta()?),
        "item/started" | "item/completed" => item_event(method, &params["item"])?,
        "turn/started" => {
            let mut turn = turn.lock().unwrap();
            if turn.done.is_some() {
                turn.id = params["turn"]["id"].as_str().map(str::to_string);
            }
            return None;
        }
        "turn/completed" => {
            let done = std::mem::take(&mut *turn.lock().unwrap()).done;
            if let Some(done) = done {
                let _ = done.send(turn_outcome(&params["turn"]));
            }
            return None;
        }
        // Early signs of trouble, including protocol drift.
        "error" | "warning" | "deprecationNotice" | "configWarning" => {
            let message = params["error"]["message"]
                .as_str()
                .or(params["message"].as_str())
                .or(params["summary"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| params.to_string());
            Event::Warning(truncate(format!("{method}: {message}"), MAX_INPUT_CHARS))
        }
        "thread/compacted" | "model/rerouted" => Event::Other(method.to_string()),
        // Output deltas, account and status updates, and the like.
        _ => return None,
    })
}

fn turn_outcome(turn: &Value) -> Result<String> {
    let status = turn["status"].as_str().unwrap_or("unknown");
    if status == "failed" {
        let message = turn["error"]["message"].as_str().unwrap_or("no details");
        bail!("the turn failed: {message}");
    }
    Ok(status.to_string())
}

/// A tool-like item as a tool event; messages and reasoning are not tools.
fn item_event(method: &str, item: &Value) -> Option<Event> {
    let kind = item["type"].as_str()?;
    let (title, input) = match kind {
        "userMessage" | "agentMessage" | "reasoning" | "hookPrompt" | "plan" => return None,
        "commandExecution" => (item["command"].as_str().unwrap_or(kind).to_string(), None),
        "dynamicToolCall" => (
            item["tool"].as_str().unwrap_or(kind).to_string(),
            Some(&item["arguments"]),
        ),
        "mcpToolCall" => (
            format!(
                "{}/{}",
                item["server"].as_str().unwrap_or_default(),
                item["tool"].as_str().unwrap_or_default()
            ),
            Some(&item["arguments"]),
        ),
        "webSearch" => (item["query"].as_str().unwrap_or(kind).to_string(), None),
        _ => (kind.to_string(), None),
    };
    let status = match item["status"].as_str() {
        Some("inProgress") => "in_progress",
        Some(status) => status,
        None if method == "item/started" => "in_progress",
        None => "completed",
    };
    let mut fields = json!({
        "title": truncate(title, MAX_INPUT_CHARS),
        "kind": kind,
        "status": status,
    });
    if let Some(input) = input.filter(|v| !v.is_null()) {
        fields["input"] = truncate(input.to_string(), MAX_INPUT_CHARS).into();
    }
    Some(Event::Tool(item["id"].as_str()?.to_string(), fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_notifications() {
        let turn = Mutex::new(Turn::default());
        let e = notification_event(
            "item/agentMessage/delta",
            &json!({ "delta": "hi", "itemId": "i" }),
            &turn,
        );
        assert!(matches!(e, Some(Event::Text(t)) if t == "hi"));
        let e = notification_event(
            "item/reasoning/summaryTextDelta",
            &json!({ "delta": "hmm" }),
            &turn,
        );
        assert!(matches!(e, Some(Event::Thought(t)) if t == "hmm"));
        let e = notification_event(
            "item/started",
            &json!({ "item": { "type": "dynamicToolCall", "id": "c1", "tool": "bbs_send",
                "arguments": { "to": "Theo" }, "status": "inProgress" } }),
            &turn,
        );
        let Some(Event::Tool(id, fields)) = e else {
            panic!("{e:?}")
        };
        assert_eq!(id, "c1");
        assert_eq!(fields["title"], "bbs_send");
        assert_eq!(fields["status"], "in_progress");
        assert_eq!(fields["input"], "{\"to\":\"Theo\"}");
        let e = notification_event(
            "item/completed",
            &json!({ "item": { "type": "commandExecution", "id": "c2", "command": "ls",
                "status": "failed" } }),
            &turn,
        );
        assert!(
            matches!(e, Some(Event::Tool(_, f)) if f["title"] == "ls" && f["status"] == "failed")
        );
        let e = notification_event(
            "item/completed",
            &json!({ "item": { "type": "agentMessage", "id": "m", "text": "hi" } }),
            &turn,
        );
        assert!(e.is_none());
        let e = notification_event(
            "error",
            &json!({ "error": { "message": "stream lost" }, "willRetry": true }),
            &turn,
        );
        assert!(matches!(e, Some(Event::Warning(m)) if m == "error: stream lost"));
        let e = notification_event(
            "item/commandExecution/outputDelta",
            &json!({ "delta": "x" }),
            &turn,
        );
        assert!(e.is_none());
    }

    #[test]
    fn tracks_the_running_turn() {
        let turn = Mutex::new(Turn::default());
        // Not waiting for a turn: a stray turn/started is not tracked.
        notification_event("turn/started", &json!({ "turn": { "id": "t0" } }), &turn);
        assert!(turn.lock().unwrap().id.is_none());

        let (tx, mut rx) = oneshot::channel();
        turn.lock().unwrap().done = Some(tx);
        notification_event("turn/started", &json!({ "turn": { "id": "t1" } }), &turn);
        assert_eq!(turn.lock().unwrap().id.as_deref(), Some("t1"));
        notification_event(
            "turn/completed",
            &json!({ "turn": { "id": "t1", "status": "interrupted", "items": [] } }),
            &turn,
        );
        assert_eq!(rx.try_recv().unwrap().unwrap(), "interrupted");
        assert!(turn.lock().unwrap().id.is_none());

        let (tx, mut rx) = oneshot::channel();
        turn.lock().unwrap().done = Some(tx);
        notification_event(
            "turn/completed",
            &json!({ "turn": { "id": "t2", "status": "failed",
                "error": { "message": "quota" }, "items": [] } }),
            &turn,
        );
        let err = rx.try_recv().unwrap().unwrap_err();
        assert_eq!(err.to_string(), "the turn failed: quota");
    }

    #[test]
    fn approves_and_refuses_requests() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let r = answer(
            "item/commandExecution/requestApproval",
            &json!({ "command": "rm -rf build" }),
            &tx,
        )
        .unwrap();
        assert_eq!(r["decision"], "accept");
        assert!(matches!(rx.try_recv(), Ok(Event::Permission(p)) if p["title"] == "rm -rf build"));
        assert_eq!(
            answer("applyPatchApproval", &json!({}), &tx).unwrap()["decision"],
            "approved"
        );
        assert!(answer("item/tool/requestUserInput", &json!({}), &tx).is_err());
    }

    #[test]
    fn declares_the_bbs_tools_as_functions() {
        let tools = dynamic_tools();
        let tools = tools.as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "bbs_send"));
        for tool in tools {
            assert_eq!(tool["type"], "function");
            assert!(tool["inputSchema"].is_object());
            assert!(tool.get("namespace").is_none());
        }
    }
}
