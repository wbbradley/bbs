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
use tokio::sync::Notify;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant, timeout, timeout_at};

use super::effort;
use super::harness::{
    EffortUnconfirmed, Event, MAX_INPUT_CHARS, PromptNotStarted, SETUP_TIMEOUT, truncate,
};
use super::mcp::{Tools, tool_list};
use super::rpc::{Incoming, Process, Rpc};
use super::settings::HarnessConfig;
use super::{context, input};
use crate::proto::effort_outcome::Result as Outcome;
use crate::proto::{EffortRequest, EffortState};

/// Longest wait for a starting turn's id before steering or interrupting it.
const TURN_ID_WAIT: Duration = Duration::from_secs(10);

/// The running turn, and a signal for each change to it.
#[derive(Default)]
struct TurnState {
    inputs: input::Requests,
    turn: Mutex<Turn>,
    dispatch: Mutex<()>,
    changed: Notify,
}

impl TurnState {
    fn set(&self, f: impl FnOnce(&mut Turn)) {
        f(&mut self.turn.lock().unwrap());
        self.changed.notify_waiters();
    }

    /// The running turn's id. A turn that is starting may not have one yet;
    /// wait up to `wait` for it.
    async fn running_id(&self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let turn = self.turn.lock().unwrap();
                if turn.id.is_some() || turn.done.is_none() {
                    return turn.id.clone();
                }
            }
            timeout_at(deadline, changed).await.ok()?;
        }
    }
}

/// The running turn, if any.
#[derive(Default)]
struct Turn {
    cancel_requested: bool,
    /// Known once `turn/started` or the `turn/start` response arrives.
    id: Option<String>,
    /// Resolved by `turn/completed`.
    done: Option<oneshot::Sender<Result<String>>>,
    text_seen: std::collections::HashSet<String>,
    text_done: std::collections::HashSet<String>,
    before_ack: Vec<Queued>,
    latest_id: Option<String>,
    usage_invalid: bool,
    exited: bool,
}

enum Queued {
    Notification(String, Value),
    Input(Rpc, Value, Value),
}

struct CodexEffort {
    parent: Option<String>,
    state: EffortState,
    models: Vec<Value>,
    model: String,
    current: Option<String>,
}

impl Default for CodexEffort {
    fn default() -> Self {
        Self {
            parent: None,
            state: effort::unavailable("model effort metadata unavailable"),
            models: Vec::new(),
            model: String::new(),
            current: None,
        }
    }
}

impl CodexEffort {
    fn notification(&mut self, params: &Value) -> bool {
        if !self
            .parent
            .as_deref()
            .is_some_and(|id| params["threadId"].as_str() == Some(id))
        {
            return false;
        }
        let settings = &params["threadSettings"];
        if let Some(model) = settings["model"].as_str() {
            self.model = model.into();
        }
        self.current = settings["effort"].as_str().map(str::to_string);
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        let choices = self
            .models
            .iter()
            .find(|m| m["model"] == self.model || m["id"] == self.model)
            .and_then(|m| m["supportedReasoningEfforts"].as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|item| {
                        let value = item["reasoningEffort"].as_str()?;
                        Some(effort::choice(value, value, false, true))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.state = EffortState {
            supported: !choices.is_empty(),
            choices,
            current: self.current.clone(),
            reason: if self.model.is_empty() {
                "model effort metadata unavailable".into()
            } else {
                String::new()
            },
        };
    }
}

/// One running app-server and the thread it serves.
#[derive(Clone)]
pub struct Codex {
    rpc: Rpc,
    effort: Arc<Mutex<CodexEffort>>,
    events: mpsc::UnboundedSender<Event>,
    thread_id: String,
    pub resumed: bool,
    turn: Arc<TurnState>,
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
        input_enabled: bool,
    ) -> Result<(Codex, Process, mpsc::UnboundedReceiver<Event>)> {
        let (events, rx) = mpsc::unbounded_channel();
        let turn = Arc::new(TurnState::default());
        turn.inputs.enable(input_enabled);
        let effort = Arc::new(Mutex::new(CodexEffort::default()));
        let handler = {
            let turn = turn.clone();
            let tools = Arc::new(tokio::sync::Mutex::new(tools));
            let effort = effort.clone();
            let events = events.clone();
            move |rpc: &Rpc, msg: Incoming| handle(rpc, msg, &turn, &tools, &effort, &events)
        };
        let (rpc, mut process) = Rpc::spawn(&config.command, &config.env, cwd, stderr, handler)?;
        let mut codex = Codex {
            rpc,
            effort,
            events,
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
        // Sent on resume too, so config changes apply; the dynamic tools are
        // sent only on thread/start, since a resumed thread keeps them.
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
            self.effort.lock().unwrap().parent = Some(id.to_string());
            let mut resume_params = params.clone();
            resume_params["threadId"] = id.into();
            match self.rpc.request("thread/resume", resume_params).await {
                Ok(result) => {
                    self.discover_effort(&result).await;
                    self.thread_id = id.to_string();
                    self.resumed = true;
                    return Ok(());
                }
                Err(err) => tracing::warn!("resuming codex thread {id} failed: {err:#}"),
            }
        }
        self.effort.lock().unwrap().parent = None;
        params["dynamicTools"] = dynamic_tools();
        let result = self.rpc.request("thread/start", params).await?;
        self.thread_id = result["thread"]["id"]
            .as_str()
            .context("thread/start returned no thread id")?
            .to_string();
        self.effort.lock().unwrap().parent = Some(self.thread_id.clone());
        self.discover_effort(&result).await;
        Ok(())
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn effort_state(&self) -> EffortState {
        self.effort.lock().unwrap().state.clone()
    }

    async fn discover_effort(&self, opened: &Value) {
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let result = timeout(
                effort::SET_TIMEOUT,
                self.rpc.request("model/list", json!({ "cursor": cursor })),
            )
            .await;
            let Ok(Ok(page)) = result else {
                break;
            };
            if let Some(data) = page["data"].as_array() {
                models.extend(data.iter().cloned());
            }
            cursor = page["nextCursor"].as_str().map(str::to_string);
            match &cursor {
                Some(c) if seen.insert(c.clone()) => {}
                _ => break,
            }
            if seen.len() >= 32 {
                break;
            }
        }
        let mut e = self.effort.lock().unwrap();
        e.models = models;
        e.model = opened["model"].as_str().unwrap_or_default().into();
        e.current = opened["reasoningEffort"].as_str().map(str::to_string);
        e.refresh();
    }

    pub fn inputs(&self) -> &input::Requests {
        &self.turn.inputs
    }
    pub fn cancel_input(&self) {
        self.turn.inputs.cancel(&self.events);
    }
    pub async fn answer_input(
        &self,
        key: &str,
        action: crate::proto::AnswerAction,
        answers: &[crate::proto::InputAnswer],
    ) -> Result<()> {
        self.turn
            .inputs
            .answer(key, action, answers, &self.events)
            .await
    }

    /// Run one turn; returns its status (`completed` or `interrupted`).
    pub async fn prompt(&self, text: &str, effort: Option<EffortRequest>) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.events.send(Event::ContextChanged(None));
        self.turn.set(|turn| {
            *turn = Turn {
                done: Some(tx),
                ..Turn::default()
            }
        });
        let mut params = json!({ "threadId": self.thread_id, "input": input(text) });
        let effort =
            effort.filter(
                |request| match effort::validate(&self.effort_state(), &request.value) {
                    Ok(()) => true,
                    Err(err) => {
                        let _ = self.events.send(Event::EffortOutcome(effort::outcome(
                            request.request_id,
                            Outcome::Failed,
                            err.to_string(),
                        )));
                        false
                    }
                },
            );
        if let Some(request) = &effort {
            params["effort"] = request.value.clone().into();
        }
        // Only the start ACK is bounded, not the duration of the model's turn.
        let changing_effort = effort.is_some();
        let call = self.rpc.call("turn/start", params);
        let result = if changing_effort {
            timeout(effort::SET_TIMEOUT, call).await
        } else {
            Ok(call.await)
        };
        let result = match result {
            Ok(Ok(result)) => {
                if let Some(request) = effort {
                    let mut e = self.effort.lock().unwrap();
                    e.current = Some(request.value);
                    e.refresh();
                    let _ = self.events.send(Event::EffortOutcome(effort::outcome(
                        request.request_id,
                        Outcome::Applied,
                        "",
                    )));
                }
                result
            }
            other => {
                let (definite, message) = match other {
                    Ok(Err(err)) => (effort::definite_rejection(err.code), err.message),
                    _ => (
                        false,
                        "turn/start acknowledgment timed out; prompt acceptance is unknown".into(),
                    ),
                };
                if let Some(request) = effort {
                    if !definite {
                        let mut e = self.effort.lock().unwrap();
                        e.current = None;
                        e.refresh();
                        e.state.reason = effort::bounded_detail(&message);
                    }
                    let _ = self.events.send(Event::EffortOutcome(effort::outcome(
                        request.request_id,
                        if definite {
                            Outcome::Failed
                        } else {
                            Outcome::Unknown
                        },
                        message.clone(),
                    )));
                }
                self.turn.set(|turn| {
                    for q in std::mem::take(&mut turn.before_ack) {
                        if let Queued::Input(rpc, id, _) = q {
                            rpc.respond_later(id, Err("turn did not start".into()));
                        }
                    }
                    *turn = Turn::default();
                });
                if definite && changing_effort {
                    return Err(PromptNotStarted(message).into());
                }
                if changing_effort {
                    return Err(EffortUnconfirmed {
                        message,
                        prompt_sent: true,
                    }
                    .into());
                }
                return Err(anyhow!(message));
            }
        };
        {
            let _ordered = self.turn.dispatch.lock().unwrap();
            let id = result["turn"]["id"]
                .as_str()
                .context("turn/start returned no turn id")?;
            let mut queued = Vec::new();
            self.turn.set(|turn| {
                turn.id = Some(id.to_string());
                queued = std::mem::take(&mut turn.before_ack);
            });
            for event in queued {
                match event {
                    Queued::Notification(method, params) => {
                        dispatch_notification(&method, &params, &self.turn, &self.events)
                    }
                    Queued::Input(rpc, id, params) => {
                        dispatch_input(&rpc, id, params, &self.turn, &self.events)
                    }
                }
            }
            self.turn.set(|turn| {
                if turn.exited
                    && let Some(done) = turn.done.take()
                {
                    let _ = done.send(Err(anyhow!("the harness exited")));
                }
            });
        }
        rx.await.map_err(|_| anyhow!("the harness exited"))?
    }

    /// Add `text` to the running turn. Fails when no turn is running or the
    /// turn cannot take input (it is ending, say).
    pub async fn steer(&self, text: &str) -> Result<()> {
        let id = self
            .turn
            .running_id(TURN_ID_WAIT)
            .await
            .context("no turn is running")?;
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
        {
            let _ordered = self.turn.dispatch.lock().unwrap();
            self.turn.set(|turn| turn.cancel_requested = true);
            self.cancel_input();
        }
        if let Some(id) = self.turn.running_id(TURN_ID_WAIT).await {
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
    turn: &Arc<TurnState>,
    tools: &Arc<tokio::sync::Mutex<Tools>>,
    effort: &Mutex<CodexEffort>,
    events: &mpsc::UnboundedSender<Event>,
) {
    match msg {
        Incoming::Request { id, method, params } if method == "item/tool/requestUserInput" => {
            let parent = effort.lock().unwrap().parent.clone();
            if parent.as_deref() != params["threadId"].as_str() || parent.is_none() {
                rpc.respond_later(
                    id,
                    Err("human input must belong to the active parent".into()),
                );
                return;
            }
            let _ordered = turn.dispatch.lock().unwrap();
            dispatch_input(rpc, id, params, turn, events);
        }
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
            if method == "thread/settings/updated" {
                let mut e = effort.lock().unwrap();
                let model = e.model.clone();
                if e.notification(&params) {
                    let _ = events.send(Event::EffortChanged);
                    if model != e.model {
                        turn.set(|t| {
                            t.usage_invalid = true;
                            t.latest_id = None;
                        });
                        let _ = events.send(Event::ContextChanged(None));
                    }
                }
            }
            // Only the opened parent's live turn can produce owner chat.
            // History replay and child notifications never enter the text stream.
            if effort
                .lock()
                .unwrap()
                .parent
                .as_deref()
                .is_none_or(|id| params["threadId"].as_str() != Some(id))
            {
                return;
            }
            let _ordered = turn.dispatch.lock().unwrap();
            dispatch_notification(&method, &params, turn, events);
        }
        Incoming::Exited => {
            let _ordered = turn.dispatch.lock().unwrap();
            turn.inputs.cancel(events);
            turn.set(|turn| {
                turn.exited = true;
                // A received start ACK may still be awaiting its task. Keep
                // pre-ACK text until that task binds and drains it.
                if (turn.id.is_some() || turn.before_ack.is_empty())
                    && let Some(done) = turn.done.take()
                {
                    let _ = done.send(Err(anyhow!("the harness exited")));
                }
            });
            let _ = events.send(Event::Exited);
        }
    }
}

/// Defer pre-ACK callbacks in wire order without blocking the JSON-RPC reader.
fn dispatch_input(
    rpc: &Rpc,
    id: Value,
    params: Value,
    turn: &TurnState,
    events: &mpsc::UnboundedSender<Event>,
) {
    let mut current = turn.turn.lock().unwrap();
    if current.done.is_some()
        && !current.exited
        && !current.cancel_requested
        && current.id.is_none()
    {
        if current
            .before_ack
            .iter()
            .any(|q| matches!(q, Queued::Input(..)))
        {
            rpc.respond_later(id, Err("another human-input request is pending".into()));
        } else {
            current
                .before_ack
                .push(Queued::Input(rpc.clone(), id, params));
        }
        return;
    }
    let valid = current.done.is_some()
        && !current.exited
        && !current.cancel_requested
        && current.id.as_deref() == params["turnId"].as_str()
        && current.id.is_some()
        && params["itemId"].as_str().is_some_and(|s| !s.is_empty());
    drop(current);
    let result = if valid {
        input::codex(&params).and_then(|form| turn.inputs.open(rpc, id.clone(), form, events))
    } else {
        Err(anyhow!(
            "human input is no longer in the active parent turn"
        ))
    };
    if let Err(error) = result {
        rpc.respond_later(id, Err(error.to_string()));
    }
}

/// Called under `dispatch`, including buffered notifications that preceded the
/// turn/start ACK. A previous turn cannot finish or speak for the current one.
fn dispatch_notification(
    method: &str,
    params: &Value,
    turn: &TurnState,
    events: &mpsc::UnboundedSender<Event>,
) {
    if method == "serverRequest/resolved" {
        let mut current = turn.turn.lock().unwrap();
        if current.done.is_some() && current.id.is_none() {
            // Remove the deferred callback as well as an already opened form.
            current
                .before_ack
                .retain(|q| !matches!(q, Queued::Input(_, id, _) if id == &params["requestId"]));
        }
        drop(current);
        turn.inputs.withdraw(&params["requestId"], events);
        return;
    }
    if method.starts_with("item/")
        || method.starts_with("turn/")
        || matches!(
            method,
            "thread/tokenUsage/updated" | "thread/compacted" | "model/rerouted"
        )
    {
        let incoming = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str());
        let mut current = turn.turn.lock().unwrap();
        if incoming.is_none() {
            return;
        }
        if matches!(
            method,
            "thread/tokenUsage/updated" | "thread/compacted" | "model/rerouted"
        ) {
            if method == "thread/tokenUsage/updated" && current.usage_invalid {
                return;
            }
            if current.done.is_none() {
                if incoming != current.latest_id.as_deref() {
                    return;
                }
                drop(current);
                if let Some(event) = notification_event(method, params, turn) {
                    let _ = events.send(event);
                }
                return;
            }
        } else if current.done.is_none() {
            return;
        }
        let Some(expected) = current.id.as_deref() else {
            // Bind only to the response for our own turn/start, not an arbitrary
            // turn/started notification from the parent thread.
            current
                .before_ack
                .push(Queued::Notification(method.to_string(), params.clone()));
            return;
        };
        if incoming != Some(expected) {
            return;
        }
    }
    if method == "turn/completed" {
        turn.inputs.cancel(events);
    }
    if method == "item/completed" && params["item"]["type"] == "contextCompaction" {
        turn.set(|t| {
            t.usage_invalid = true;
            t.latest_id = None;
        });
        let _ = events.send(Event::ContextChanged(None));
    }
    if let Some(event) = notification_event(method, params, turn) {
        let _ = events.send(event);
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
fn notification_event(method: &str, params: &Value, turn: &TurnState) -> Option<Event> {
    let delta = || params["delta"].as_str().map(str::to_string);
    let source = |message: Option<&str>| super::outbox::Source {
        session: params["threadId"].as_str().unwrap_or_default().to_string(),
        turn: params["turnId"].as_str().map(str::to_string),
        message: message.map(str::to_string),
    };
    Some(match method {
        "item/agentMessage/delta" => {
            let id = params["itemId"].as_str()?;
            let text = delta()?;
            if text.is_empty() {
                return None;
            }
            let mut state = turn.turn.lock().unwrap();
            if state.text_done.contains(id) {
                return None;
            }
            state.text_seen.insert(id.into());
            Event::Text(source(Some(id)), text)
        }
        "item/completed" if params["item"]["type"] == "agentMessage" => {
            let id = params["item"]["id"].as_str()?;
            let mut state = turn.turn.lock().unwrap();
            if !state.text_done.insert(id.into()) {
                return None;
            }
            let fallback = if state.text_seen.contains(id) {
                None
            } else {
                params["item"]["text"].as_str().map(str::to_string)
            };
            Event::TextEnd(source(Some(id)), fallback)
        }
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => Event::Thought(delta()?),
        "item/started" | "item/completed" => item_event(method, &params["item"])?,
        "turn/started" => {
            turn.set(|turn| {
                if turn.done.is_some() {
                    turn.id = params["turn"]["id"].as_str().map(str::to_string);
                }
            });
            return None;
        }
        "thread/tokenUsage/updated" => Event::ContextChanged(context::codex(params)),
        "thread/compacted" | "model/rerouted" => {
            turn.set(|t| {
                t.usage_invalid = true;
                t.latest_id = None;
            });
            Event::ContextChanged(None)
        }
        "turn/completed" => {
            let mut done = None;
            turn.set(|turn| {
                let old = std::mem::take(turn);
                turn.latest_id = old.id;
                turn.usage_invalid = old.usage_invalid;
                done = old.done;
            });
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
    fn context_accepts_latest_completed_then_rejects_older_or_invalidated_usage() {
        let state = TurnState::default();
        let (done, _result) = oneshot::channel();
        state.set(|s| {
            s.id = Some("t1".into());
            s.done = Some(done);
        });
        let (events, mut rx) = mpsc::unbounded_channel();
        let usage = |id| json!({"turnId":id,"tokenUsage":{"last":{"totalTokens":42},"modelContextWindow":100}});
        dispatch_notification(
            "turn/completed",
            &json!({"turn":{"id":"t1","status":"completed"}}),
            &state,
            &events,
        );
        dispatch_notification("thread/tokenUsage/updated", &usage("t1"), &state, &events);
        assert!(matches!(rx.try_recv(), Ok(Event::ContextChanged(Some(_)))));
        dispatch_notification(
            "thread/tokenUsage/updated",
            &usage("older"),
            &state,
            &events,
        );
        assert!(rx.try_recv().is_err());
        dispatch_notification("thread/compacted", &json!({"turnId":"t1"}), &state, &events);
        assert!(matches!(rx.try_recv(), Ok(Event::ContextChanged(None))));
        dispatch_notification("thread/tokenUsage/updated", &usage("t1"), &state, &events);
        assert!(rx.try_recv().is_err());
        let (done, _result) = oneshot::channel();
        state.set(|s| {
            *s = Turn {
                id: Some("t2".into()),
                done: Some(done),
                ..Default::default()
            }
        });
        dispatch_notification("thread/tokenUsage/updated", &usage("t1"), &state, &events);
        assert!(rx.try_recv().is_err());
        dispatch_notification("thread/tokenUsage/updated", &usage("t2"), &state, &events);
        assert!(matches!(rx.try_recv(), Ok(Event::ContextChanged(Some(_)))));
        dispatch_notification("model/rerouted", &json!({"turnId":"t2"}), &state, &events);
        assert!(matches!(rx.try_recv(), Ok(Event::ContextChanged(None))));
        dispatch_notification("thread/tokenUsage/updated", &usage("t2"), &state, &events);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn current_compaction_item_clears_usage_but_stale_invalidations_do_not() {
        let state = TurnState::default();
        let (done, _) = oneshot::channel();
        state.set(|s| {
            s.id = Some("live".into());
            s.done = Some(done);
        });
        let (events, mut rx) = mpsc::unbounded_channel();
        for method in ["thread/compacted", "model/rerouted"] {
            dispatch_notification(method, &json!({"turnId":"old"}), &state, &events);
        }
        assert!(rx.try_recv().is_err());
        assert!(!state.turn.lock().unwrap().usage_invalid);
        dispatch_notification(
            "item/completed",
            &json!({"turnId":"live","item":{"id":"compaction","type":"contextCompaction"}}),
            &state,
            &events,
        );
        assert!(matches!(rx.try_recv(), Ok(Event::ContextChanged(None))));
        assert!(state.turn.lock().unwrap().usage_invalid);
        assert!(
            matches!(rx.try_recv(), Ok(Event::Tool(_, _))),
            "retain existing activity semantics"
        );
    }

    #[test]
    fn effort_metadata_ignores_foreign_and_missing_thread_ids() {
        let mut e = CodexEffort::default();
        let update = |id: &str, model: &str, level: &str| {
            json!({
                "threadId":id, "threadSettings":{"model":model,"effort":level}
            })
        };
        assert!(!e.notification(&update("parent", "m", "high")));
        e.parent = Some("parent".into());
        assert!(e.notification(&update("parent", "m", "high")));
        assert!(!e.notification(&update("child", "other", "max")));
        assert!(!e.notification(&json!({"threadSettings":{"effort":"low"}})));
        assert_eq!(e.state.current.as_deref(), Some("high"));
        assert_eq!(e.model, "m");
        assert!(e.notification(&update("parent", "m", "low")));
        assert_eq!(e.state.current.as_deref(), Some("low"));
    }

    #[test]
    fn effort_current_is_not_the_catalog_default_or_choice_order() {
        let mut e = CodexEffort {
            model: "m".into(),
            models: vec![json!({
                "model":"m", "defaultReasoningEffort":"medium", "supportedReasoningEfforts":[
                    {"reasoningEffort":"max"}, {"reasoningEffort":"low"}, {"reasoningEffort":"ultra"}
                ]
            })],
            ..Default::default()
        };
        e.refresh();
        assert!(e.state.supported);
        assert!(e.state.current.is_none());
        assert_eq!(e.state.choices[0].value, "max");
        assert!(e.state.choices[0].rank > e.state.choices[1].rank);
        assert_eq!(e.state.choices[2].rank, None);
        e.current = Some("low".into());
        e.refresh();
        assert_eq!(e.state.current.as_deref(), Some("low"));
        e.model = "other".into();
        e.refresh();
        assert!(!e.state.supported);
    }

    #[test]
    fn stale_turns_cannot_speak_or_complete_the_current_turn() {
        let turn = TurnState::default();
        let (done, mut result) = oneshot::channel();
        turn.set(|s| {
            s.id = Some("current".into());
            s.done = Some(done);
        });
        let (events, mut rx) = mpsc::unbounded_channel();
        dispatch_notification(
            "item/agentMessage/delta",
            &json!({ "threadId": "parent", "turnId": "old", "itemId": "m", "delta": "stale" }),
            &turn,
            &events,
        );
        dispatch_notification(
            "turn/completed",
            &json!({ "threadId": "parent", "turn": { "id": "old", "status": "completed" } }),
            &turn,
            &events,
        );
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        // A malformed delta must not suppress the completed-item fallback.
        dispatch_notification(
            "item/agentMessage/delta",
            &json!({ "turnId": "current", "itemId": "m" }),
            &turn,
            &events,
        );
        dispatch_notification(
            "item/agentMessage/delta",
            &json!({ "turnId": "current", "itemId": "m", "delta": "" }),
            &turn,
            &events,
        );
        dispatch_notification(
            "item/completed",
            &json!({ "turnId": "current", "item": { "type": "agentMessage", "id": "m", "text": "actual" } }),
            &turn,
            &events,
        );
        assert!(matches!(rx.try_recv(), Ok(Event::TextEnd(_, Some(t))) if t == "actual"));
        dispatch_notification(
            "turn/completed",
            &json!({ "turn": { "id": "current", "status": "completed" } }),
            &turn,
            &events,
        );
        assert_eq!(result.try_recv().unwrap().unwrap(), "completed");
    }

    #[test]
    fn maps_notifications() {
        let turn = TurnState::default();
        let e = notification_event(
            "item/agentMessage/delta",
            &json!({ "delta": "hi", "itemId": "i" }),
            &turn,
        );
        assert!(matches!(e, Some(Event::Text(_, t)) if t == "hi"));
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
        assert!(matches!(e, Some(Event::TextEnd(_, Some(t))) if t == "hi"));
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
        let turn = TurnState::default();
        // Not waiting for a turn: a stray turn/started is not tracked.
        notification_event("turn/started", &json!({ "turn": { "id": "t0" } }), &turn);
        assert!(turn.turn.lock().unwrap().id.is_none());

        let (tx, mut rx) = oneshot::channel();
        turn.turn.lock().unwrap().done = Some(tx);
        notification_event("turn/started", &json!({ "turn": { "id": "t1" } }), &turn);
        assert_eq!(turn.turn.lock().unwrap().id.as_deref(), Some("t1"));
        notification_event(
            "turn/completed",
            &json!({ "turn": { "id": "t1", "status": "interrupted", "items": [] } }),
            &turn,
        );
        assert_eq!(rx.try_recv().unwrap().unwrap(), "interrupted");
        assert!(turn.turn.lock().unwrap().id.is_none());

        let (tx, mut rx) = oneshot::channel();
        turn.turn.lock().unwrap().done = Some(tx);
        notification_event(
            "turn/completed",
            &json!({ "turn": { "id": "t2", "status": "failed",
                "error": { "message": "quota" }, "items": [] } }),
            &turn,
        );
        let err = rx.try_recv().unwrap().unwrap_err();
        assert_eq!(err.to_string(), "the turn failed: quota");
    }

    #[tokio::test]
    async fn waits_for_a_starting_turns_id() {
        let turn = Arc::new(TurnState::default());
        let wait = Duration::from_secs(5);
        assert_eq!(turn.running_id(wait).await, None, "no turn running");

        let (tx, _rx) = oneshot::channel();
        turn.set(|t| t.done = Some(tx));
        assert_eq!(turn.running_id(Duration::from_millis(50)).await, None);
        let started = turn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            notification_event("turn/started", &json!({ "turn": { "id": "t1" } }), &started);
        });
        assert_eq!(turn.running_id(wait).await.as_deref(), Some("t1"));

        // A turn that ends before its id arrives has nothing left to steer.
        turn.set(|t| *t = Turn::default());
        let (tx, _rx) = oneshot::channel();
        turn.set(|t| t.done = Some(tx));
        let ended = turn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ended.set(|t| *t = Turn::default());
        });
        assert_eq!(turn.running_id(wait).await, None);
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
