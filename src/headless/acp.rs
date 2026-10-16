//! A small Agent Client Protocol client over a child process's stdio.
//!
//! It implements only what headless needs: initialize, new/resume/load a
//! session, set session config options, prompt, and cancel. Permission requests
//! are approved. Session updates become [`Event`]s; anything unrecognized is
//! reported by kind rather than rejected, so harness extensions do not break it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::harness::{Event, MAX_INPUT_CHARS, SETUP_TIMEOUT, truncate};
use super::rpc::{Incoming, Process, Rpc};
use super::settings::HarnessConfig;

const PROTOCOL_VERSION: u64 = 1;
/// JSON-RPC error code ACP uses when the agent needs `authenticate` first.
const AUTH_REQUIRED: i64 = -32000;
/// The MCP server the harness should start for the agent's BBS tools.
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServer {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "env": self.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
        })
    }
}

/// One running ACP agent and the session it serves.
#[derive(Clone)]
pub struct Acp {
    rpc: Rpc,
    /// While a loaded session replays its history, updates are not events.
    replaying: Arc<AtomicBool>,
    session_id: String,
    pub session_resumed: bool,
}

/// What the agent advertised at `initialize`.
struct AgentCaps {
    load: bool,
    resume: bool,
}

impl Acp {
    /// Start the harness, initialize it, and open a session: resume `resume`
    /// when the agent can, else start a new one.
    pub async fn start(
        config: &HarnessConfig,
        cwd: &Path,
        stderr: &Path,
        mcp: &McpServer,
        resume: Option<&str>,
    ) -> Result<(Acp, Process, mpsc::UnboundedReceiver<Event>, Vec<String>)> {
        let (events, rx) = mpsc::unbounded_channel();
        let replaying = Arc::new(AtomicBool::new(false));
        let handler = {
            let replaying = replaying.clone();
            move |rpc: &Rpc, msg: Incoming| handle(rpc, msg, &replaying, &events)
        };
        let (rpc, mut process) = Rpc::spawn(&config.command, &config.env, cwd, stderr, handler)?;
        let mut acp = Acp {
            rpc,
            replaying,
            session_id: String::new(),
            session_resumed: false,
        };
        let setup = timeout(SETUP_TIMEOUT, acp.open_session(config, cwd, mcp, resume)).await;
        match setup {
            Ok(Ok(warnings)) => Ok((acp, process, rx, warnings)),
            Ok(Err(err)) => {
                process.stop().await;
                Err(err)
            }
            Err(_) => {
                process.stop().await;
                bail!("the harness did not open a session within {SETUP_TIMEOUT:?}")
            }
        }
    }

    async fn open_session(
        &mut self,
        config: &HarnessConfig,
        cwd: &Path,
        mcp: &McpServer,
        resume: Option<&str>,
    ) -> Result<Vec<String>> {
        let acp = self;
        let caps = acp.initialize().await?;
        let cwd = cwd.display().to_string();
        let servers = json!([mcp.to_json()]);
        let mut opened = None;
        if let Some(id) = resume {
            let params = json!({ "sessionId": id, "cwd": cwd, "mcpServers": servers });
            let method = if caps.resume {
                Some("session/resume")
            } else if caps.load {
                Some("session/load")
            } else {
                None
            };
            if let Some(method) = method {
                acp.replaying.store(true, Ordering::SeqCst);
                let result = acp.rpc.request(method, params).await;
                acp.replaying.store(false, Ordering::SeqCst);
                match result {
                    Ok(result) => {
                        acp.session_id = id.to_string();
                        acp.session_resumed = true;
                        opened = Some(result);
                    }
                    Err(err) => tracing::warn!("resuming harness session {id} failed: {err:#}"),
                }
            }
        }
        let result = match opened {
            Some(result) => result,
            None => {
                let params = json!({ "cwd": cwd, "mcpServers": servers });
                let result = match acp.rpc.call("session/new", params).await {
                    // Logging in may be interactive; it belongs outside headless.
                    Err(err) if err.code == AUTH_REQUIRED => bail!(
                        "the harness needs a login; run it interactively once to log in ({})",
                        err.message
                    ),
                    other => other.map_err(|e| anyhow!("session/new: {}", e.message))?,
                };
                acp.session_id = result["sessionId"]
                    .as_str()
                    .context("session/new returned no sessionId")?
                    .to_string();
                result
            }
        };
        acp.configure(&result, config).await
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn initialize(&self) -> Result<AgentCaps> {
        let result = self
            .rpc
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false,
                    },
                    "clientInfo": { "name": "bbs", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        let version = result["protocolVersion"].as_u64().unwrap_or(0);
        if version != PROTOCOL_VERSION {
            bail!("harness speaks ACP version {version}; bbs supports {PROTOCOL_VERSION}");
        }
        let caps = &result["agentCapabilities"];
        Ok(AgentCaps {
            load: caps["loadSession"].as_bool().unwrap_or(false),
            resume: caps["sessionCapabilities"]["resume"].is_object(),
        })
    }

    /// Apply the configured model and session config options to the session
    /// `opened` describes (a session/new, resume, or load result). A resumed
    /// session may not describe its options; it keeps the model it had, so a
    /// missing model option is a warning there and an error for a new session.
    async fn configure(&self, opened: &Value, config: &HarnessConfig) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let options = opened["configOptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(model) = &config.model {
            if let Some(option) = options.iter().find(|o| o["category"] == "model") {
                let id = option["id"]
                    .as_str()
                    .context("model option without an id")?;
                self.set_option(id, model).await?;
            } else if opened["models"].is_object() {
                // The pre-config-option model selector some agents still use.
                self.rpc
                    .request(
                        "session/set_model",
                        json!({ "sessionId": self.session_id, "modelId": model }),
                    )
                    .await
                    .with_context(|| format!("setting model {model:?}"))?;
            } else if self.session_resumed {
                warnings.push(format!(
                    "the resumed session lists no model option; model {model:?} not applied"
                ));
            } else {
                bail!("the harness offers no model option; remove `model` from its config");
            }
        }
        for (id, value) in &config.config {
            self.set_option(id, value).await?;
        }
        Ok(warnings)
    }

    async fn set_option(&self, id: &str, value: &str) -> Result<()> {
        self.rpc
            .request(
                "session/set_config_option",
                json!({ "sessionId": self.session_id, "configId": id, "value": value }),
            )
            .await
            .with_context(|| format!("setting session option {id} to {value:?}"))?;
        Ok(())
    }

    /// Run one turn; returns the stop reason.
    pub async fn prompt(&self, text: &str) -> Result<String> {
        let result = self
            .rpc
            .request(
                "session/prompt",
                json!({
                    "sessionId": self.session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await?;
        Ok(result["stopReason"]
            .as_str()
            .unwrap_or("unknown")
            .to_string())
    }

    /// Ask the agent to stop the running turn; the prompt then returns `cancelled`.
    pub async fn cancel(&self) -> Result<()> {
        self.rpc
            .notify("session/cancel", json!({ "sessionId": self.session_id }))
            .await
    }
}

fn handle(rpc: &Rpc, msg: Incoming, replaying: &AtomicBool, events: &mpsc::UnboundedSender<Event>) {
    match msg {
        Incoming::Request { id, method, params } => {
            rpc.respond_later(id, answer(&method, &params, events));
        }
        Incoming::Notification { method, params } if method == "session/update" => {
            if !replaying.load(Ordering::SeqCst)
                && let Some(event) = update_event(&params["update"])
            {
                let _ = events.send(event);
            }
        }
        Incoming::Notification { method, .. } => {
            let _ = events.send(Event::Other(method));
        }
        Incoming::Exited => {
            let _ = events.send(Event::Exited);
        }
    }
}

/// Answer a request from the agent. Permission requests are approved; this
/// client offers no file system or terminal methods.
fn answer(
    method: &str,
    params: &Value,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<Value, String> {
    if method != "session/request_permission" {
        return Err(format!("bbs headless does not support {method}"));
    }
    let options = params["options"].as_array().cloned().unwrap_or_default();
    let rank = |o: &Value| match o["kind"].as_str() {
        Some("allow_always") => 0,
        Some("allow_once") => 1,
        _ => 2,
    };
    let Some(choice) = options
        .iter()
        .min_by_key(|o| rank(o))
        .filter(|o| rank(o) < 2)
    else {
        let _ = events.send(Event::Permission(json!({
            "title": params["toolCall"]["title"],
            "outcome": "cancelled",
        })));
        return Ok(json!({ "outcome": { "outcome": "cancelled" } }));
    };
    let _ = events.send(Event::Permission(json!({
        "title": params["toolCall"]["title"],
        "option": choice["optionId"],
    })));
    Ok(json!({ "outcome": { "outcome": "selected", "optionId": choice["optionId"] } }))
}

fn update_event(update: &Value) -> Option<Event> {
    let kind = update["sessionUpdate"].as_str()?;
    let text = || update["content"]["text"].as_str().map(str::to_string);
    Some(match kind {
        "agent_message_chunk" => Event::Text(text()?),
        "agent_thought_chunk" => Event::Thought(text()?),
        "user_message_chunk" => return None,
        "tool_call" | "tool_call_update" => {
            let id = update["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let mut fields = serde_json::Map::new();
            for key in ["title", "kind", "status"] {
                if let Some(v) = update.get(key).filter(|v| !v.is_null()) {
                    fields.insert(key.into(), v.clone());
                }
            }
            if let Some(input) = update.get("rawInput").filter(|v| !v.is_null()) {
                fields.insert(
                    "input".into(),
                    truncate(input.to_string(), MAX_INPUT_CHARS).into(),
                );
            }
            Event::Tool(id, Value::Object(fields))
        }
        other => Event::Other(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_session_updates() {
        let e = update_event(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" },
        }));
        assert!(matches!(e, Some(Event::Text(t)) if t == "hi"));
        let e = update_event(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run ls",
            "kind": "execute",
            "status": "pending",
            "rawInput": { "command": "ls" },
        }));
        let Some(Event::Tool(id, fields)) = e else {
            panic!("{e:?}")
        };
        assert_eq!(id, "t1");
        assert_eq!(fields["title"], "Run ls");
        assert_eq!(fields["input"], "{\"command\":\"ls\"}");
        assert!(update_event(&json!({ "sessionUpdate": "user_message_chunk" })).is_none());
        assert!(matches!(
            update_event(&json!({ "sessionUpdate": "plan" })),
            Some(Event::Other(k)) if k == "plan"
        ));
    }

    #[test]
    fn approves_permission_requests() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let params = json!({
            "toolCall": { "title": "Edit file" },
            "options": [
                { "optionId": "no", "kind": "reject_once" },
                { "optionId": "once", "kind": "allow_once" },
                { "optionId": "always", "kind": "allow_always" },
            ],
        });
        let r = answer("session/request_permission", &params, &tx).unwrap();
        assert_eq!(r["outcome"]["optionId"], "always");
        assert!(matches!(rx.try_recv(), Ok(Event::Permission(_))));
        let reject_only = json!({ "options": [{ "optionId": "no", "kind": "reject_once" }] });
        let r = answer("session/request_permission", &reject_only, &tx).unwrap();
        assert_eq!(r["outcome"]["outcome"], "cancelled");
        assert!(answer("fs/read_text_file", &json!({}), &tx).is_err());
    }
}
