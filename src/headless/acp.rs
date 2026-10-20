//! A small Agent Client Protocol client over a child process's stdio.
//!
//! It implements only what headless needs: initialize, new/resume/load a
//! session, set session config options, prompt, and cancel. Permission requests
//! are approved. Session updates become [`Event`]s; anything unrecognized is
//! reported by kind rather than rejected, so harness extensions do not break it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::effort;
use super::harness::{EffortUnconfirmed, Event, MAX_INPUT_CHARS, SETUP_TIMEOUT, truncate};
use super::rpc::{Incoming, Process, Rpc};
use super::settings::{HarnessConfig, McpEnv};
use super::{context, input};
use crate::proto::effort_outcome::Result as Outcome;
use crate::proto::{EffortRequest, EffortState};

const PROTOCOL_VERSION: u64 = 1;
/// JSON-RPC error code ACP uses when the agent needs `authenticate` first.
const AUTH_REQUIRED: i64 = -32000;
/// The MCP server the harness should start for the agent's BBS tools.
#[derive(Clone)]
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
    pub inputs: Arc<input::Requests>,
    input_enabled: bool,
    rpc: Rpc,
    effort: Arc<Mutex<AcpEffort>>,
    events: mpsc::UnboundedSender<Event>,
    /// While a loaded session replays its history, updates are not events.
    replaying: Arc<AtomicBool>,
    active_prompt: Arc<Mutex<ActivePrompt>>,
    session_id: String,
    pub session_resumed: bool,
}

#[derive(Default)]
struct ActivePrompt {
    id: Option<String>,
    input_cancelled: bool,
}

struct AcpEffort {
    parent: Option<String>,
    state: EffortState,
    id: Option<String>,
    claude: bool,
    model: Option<String>,
    usage_valid: bool,
}

impl Default for AcpEffort {
    fn default() -> Self {
        Self {
            parent: None,
            state: effort::unavailable("no thought-level option advertised"),
            id: None,
            claude: false,
            model: None,
            usage_valid: false,
        }
    }
}

impl AcpEffort {
    fn notification(&mut self, params: &Value) -> bool {
        if !self
            .parent
            .as_deref()
            .is_some_and(|id| params["sessionId"].as_str() == Some(id))
        {
            return false;
        }
        self.update(&params["update"]["configOptions"]);
        true
    }

    fn update(&mut self, options: &Value) {
        self.model = options
            .as_array()
            .and_then(|o| o.iter().find(|o| o["category"] == "model"))
            .and_then(|o| o["currentValue"].as_str())
            .map(str::to_string);
        self.state = effort::unavailable("no thought-level option advertised");
        self.id = None;
        let Some(option) = options.as_array().and_then(|options| {
            options
                .iter()
                .find(|o| o["category"] == "thought_level" && o["type"] == "select")
        }) else {
            return;
        };
        self.id = option["id"].as_str().map(str::to_string);
        let mut choices = Vec::new();
        if let Some(options) = option["options"].as_array() {
            for option in options {
                let values = option["options"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(std::slice::from_ref(option));
                for item in values {
                    if let Some(value) = item["value"].as_str() {
                        choices.push(effort::choice(
                            value,
                            item["name"].as_str().unwrap_or(value),
                            self.claude,
                            false,
                        ));
                    }
                }
            }
        }
        self.state = EffortState {
            supported: self.id.is_some() && !choices.is_empty(),
            choices,
            current: option["currentValue"].as_str().map(str::to_string),
            reason: String::new(),
        };
    }
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
        input_enabled: bool,
    ) -> Result<(Acp, Process, mpsc::UnboundedReceiver<Event>, Vec<String>)> {
        let mut env = config.env.clone();
        let mut mcp = mcp.clone();
        if config.mcp_env == McpEnv::Process {
            env.extend(std::mem::take(&mut mcp.env));
        }
        let (events, rx) = mpsc::unbounded_channel();
        let inputs = Arc::new(input::Requests::default());
        inputs.enable(input_enabled);
        let effort = Arc::new(Mutex::new(AcpEffort::default()));
        let replaying = Arc::new(AtomicBool::new(false));
        let active_prompt = Arc::new(Mutex::new(ActivePrompt::default()));
        let handler = {
            let inputs = inputs.clone();
            let active_prompt = active_prompt.clone();
            let replaying = replaying.clone();
            let effort = effort.clone();
            let events = events.clone();
            move |rpc: &Rpc, msg: Incoming| {
                handle(
                    rpc,
                    msg,
                    &replaying,
                    &active_prompt,
                    &effort,
                    &events,
                    &inputs,
                )
            }
        };
        let (rpc, mut process) = Rpc::spawn(&config.command, &env, cwd, stderr, handler)?;
        let mut acp = Acp {
            inputs,
            input_enabled,
            rpc,
            effort,
            events,
            replaying,
            active_prompt,
            session_id: String::new(),
            session_resumed: false,
        };
        let setup = timeout(SETUP_TIMEOUT, acp.open_session(config, cwd, &mcp, resume)).await;
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
                acp.effort.lock().unwrap().parent = Some(id.to_string());
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
                {
                    let mut effort = acp.effort.lock().unwrap();
                    effort.parent = None;
                    effort.update(&Value::Null);
                }
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
                acp.effort.lock().unwrap().parent = Some(acp.session_id.clone());
                result
            }
        };
        acp.configure(&result, config).await
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn initialize(&self) -> Result<AgentCaps> {
        let mut caps =
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false});
        if self.input_enabled {
            caps["elicitation"] = json!({"form": {}});
        }
        let result = self
            .rpc
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": caps,
                    "clientInfo": { "name": "bbs", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        let version = result["protocolVersion"].as_u64().unwrap_or(0);
        if version != PROTOCOL_VERSION {
            bail!("harness speaks ACP version {version}; bbs supports {PROTOCOL_VERSION}");
        }
        self.effort.lock().unwrap().claude =
            result["agentInfo"]["name"] == "@agentclientprotocol/claude-agent-acp";
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
        self.capture_options(opened);
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
        let result = self
            .rpc
            .request(
                "session/set_config_option",
                json!({ "sessionId": self.session_id, "configId": id, "value": value }),
            )
            .await
            .with_context(|| format!("setting session option {id} to {value:?}"))?;
        self.capture_options(&result);
        Ok(())
    }

    pub fn effort_state(&self) -> EffortState {
        self.effort.lock().unwrap().state.clone()
    }

    fn capture_options(&self, result: &Value) {
        if result.get("configOptions").is_some() {
            self.effort.lock().unwrap().update(&result["configOptions"]);
            let _ = self.events.send(Event::EffortChanged);
        }
    }

    async fn apply_effort(&self, request: EffortRequest) -> Result<()> {
        let mut uncertain = false;
        let state = self.effort_state();
        let result = if let Err(err) = effort::validate(&state, &request.value) {
            effort::outcome(request.request_id, Outcome::Failed, err.to_string())
        } else {
            let id = self.effort.lock().unwrap().id.clone();
            let call = self.rpc.call(
                "session/set_config_option",
                json!({
                    "sessionId": self.session_id, "configId": id, "value": request.value,
                }),
            );
            match timeout(effort::SET_TIMEOUT, call).await {
                Ok(Ok(result)) => {
                    // The full returned list may also change supported levels.
                    self.effort.lock().unwrap().update(&result["configOptions"]);
                    if self.effort_state().current.as_deref() == Some(request.value.as_str()) {
                        effort::outcome(request.request_id, Outcome::Applied, "")
                    } else if let Some(actual) = self.effort_state().current {
                        effort::outcome(
                            request.request_id,
                            Outcome::Failed,
                            format!(
                                "requested {:?}, provider returned {:?}",
                                request.value, actual
                            ),
                        )
                    } else {
                        effort::outcome(
                            request.request_id,
                            Outcome::Unknown,
                            "setter returned no current effort",
                        )
                    }
                }
                Ok(Err(err)) if effort::definite_rejection(err.code) => {
                    effort::outcome(request.request_id, Outcome::Failed, err.message)
                }
                other => {
                    uncertain = true;
                    let detail = match other {
                        Ok(Err(err)) => err.message,
                        _ => "effort setter timed out".into(),
                    };
                    let mut guard = self.effort.lock().unwrap();
                    guard.state.current = None;
                    guard.state.reason = effort::bounded_detail(&detail);
                    effort::outcome(request.request_id, Outcome::Unknown, detail)
                }
            }
        };
        let message = result.detail.clone();
        let _ = self.events.send(Event::EffortOutcome(result));
        if uncertain {
            return Err(EffortUnconfirmed {
                message,
                prompt_sent: false,
            }
            .into());
        }
        Ok(())
    }

    pub fn cancel_input(&self) {
        self.inputs.cancel(&self.events);
    }
    pub async fn answer_input(
        &self,
        key: &str,
        action: crate::proto::AnswerAction,
        answers: &[crate::proto::InputAnswer],
    ) -> Result<()> {
        self.inputs.answer(key, action, answers, &self.events).await
    }

    /// Run one turn; a rejected effort change never consumes its prompt mail.
    pub async fn prompt(&self, text: &str, effort: Option<EffortRequest>) -> Result<String> {
        if let Some(request) = effort {
            self.apply_effort(request).await?;
        }
        self.effort.lock().unwrap().usage_valid = true;
        let _ = self.events.send(Event::ContextChanged(None));
        {
            let mut active = self.active_prompt.lock().unwrap();
            *active = ActivePrompt {
                id: Some(crate::auth::new_token()),
                input_cancelled: false,
            };
        }
        let result = self
            .rpc
            .request(
                "session/prompt",
                json!({
                    "sessionId": self.session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await;
        self.active_prompt.lock().unwrap().id = None;
        self.cancel_input();
        let result = result?;
        Ok(result["stopReason"]
            .as_str()
            .unwrap_or("unknown")
            .to_string())
    }

    /// Ask the agent to stop the running turn; the prompt then returns `cancelled`.
    pub async fn cancel(&self) -> Result<()> {
        {
            let mut active = self.active_prompt.lock().unwrap();
            active.input_cancelled = true;
            self.cancel_input();
        }
        self.rpc
            .notify("session/cancel", json!({ "sessionId": self.session_id }))
            .await
    }
}

fn handle(
    rpc: &Rpc,
    msg: Incoming,
    replaying: &AtomicBool,
    active_prompt: &Mutex<ActivePrompt>,
    effort: &Mutex<AcpEffort>,
    events: &mpsc::UnboundedSender<Event>,
    inputs: &input::Requests,
) {
    match msg {
        Incoming::Request { id, method, params } if method == "elicitation/create" => {
            let active = active_prompt.lock().unwrap();
            let valid = !active.input_cancelled
                && !replaying.load(Ordering::SeqCst)
                && active.id.is_some()
                && effort
                    .lock()
                    .unwrap()
                    .parent
                    .as_deref()
                    .is_some_and(|p| params["sessionId"].as_str() == Some(p));
            let result = if valid {
                input::acp(&params).and_then(|form| inputs.open(rpc, id.clone(), form, events))
            } else {
                Err(anyhow!(
                    "human input must belong to the active parent session"
                ))
            };
            if let Err(error) = result {
                rpc.respond_later(id, Err(error.to_string()));
            }
        }
        Incoming::Request { id, method, params } => {
            rpc.respond_later(id, answer(&method, &params, events));
        }
        Incoming::Notification { method, params } if method == "session/update" => {
            let parent = effort.lock().unwrap().parent.clone();
            if params["update"]["sessionUpdate"] == "config_option_update" {
                let mut state = effort.lock().unwrap();
                let old_model = state.model.clone();
                if state.notification(&params) {
                    let _ = events.send(Event::EffortChanged);
                    if old_model != state.model {
                        state.usage_valid = false;
                        let _ = events.send(Event::ContextChanged(None));
                    }
                }
            } else if !replaying.load(Ordering::SeqCst)
                && parent
                    .as_deref()
                    .is_some_and(|id| params["sessionId"].as_str() == Some(id))
                && let Some(mut event) = update_event(&params["update"])
            {
                if matches!(event, Event::ContextChanged(_)) {
                    let mut state = effort.lock().unwrap();
                    if matches!(
                        params["update"]["sessionUpdate"].as_str(),
                        Some("current_model_update" | "compaction_update")
                    ) {
                        state.usage_valid = false;
                    } else if !state.usage_valid {
                        return;
                    }
                }
                if let Event::Text(source, _) = &mut event {
                    let Some(prompt) = active_prompt.lock().unwrap().id.clone() else {
                        return;
                    };
                    source.turn = Some(prompt);
                    source.session = params["sessionId"].as_str().unwrap().to_string();
                }
                let _ = events.send(event);
            }
        }
        Incoming::Notification { method, .. } => {
            let _ = events.send(Event::Other(method));
        }
        Incoming::Exited => {
            inputs.cancel(events);
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
    if kind == "usage_update" {
        return Some(Event::ContextChanged(context::acp(update)));
    }
    if matches!(kind, "current_model_update" | "compaction_update") {
        return Some(Event::ContextChanged(None));
    }
    let text = || update["content"]["text"].as_str().map(str::to_string);
    Some(match kind {
        "agent_message_chunk" => Event::Text(
            super::outbox::Source {
                message: update["messageId"].as_str().map(str::to_string),
                ..Default::default()
            },
            text()?,
        ),
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
    fn effort_metadata_requires_the_opened_parent_even_during_replay() {
        let mut e = AcpEffort::default();
        let update = |id: &str| {
            json!({ "sessionId":id, "update": { "configOptions":[{
            "id":"e", "type":"select", "category":"thought_level", "currentValue":"high",
            "options":[{"value":"high"}]
        }] } })
        };
        assert!(!e.notification(&update("parent"))); // Not bound yet.
        e.parent = Some("parent".into()); // Set before resume/load starts replay.
        assert!(!e.notification(&update("child")));
        assert!(!e.notification(&json!({"update":{"configOptions":[]}})));
        assert!(e.state.current.is_none());
        assert!(e.notification(&update("parent")));
        assert_eq!(e.state.current.as_deref(), Some("high"));
        assert!(!e.notification(&json!({"sessionId":"child", "update":{"configOptions":[]}})));
        assert!(e.state.supported);
        assert_eq!(e.state.current.as_deref(), Some("high"));
    }

    #[test]
    fn effort_options_preserve_default_grouped_choices_and_provider_rank() {
        let mut e = AcpEffort {
            claude: true,
            ..Default::default()
        };
        e.update(&json!([{
            "id":"effort-level", "category":"thought_level", "type":"select", "currentValue":"default",
            "options":[{"group":"levels", "options":[
                {"value":"default", "name":"Default"}, {"value":"max", "name":"Maximum"},
                {"value":"low", "name":"Low"}, {"value":"new", "name":"Future"}
            ]}]
        }]));
        assert_eq!(e.id.as_deref(), Some("effort-level"));
        assert_eq!(e.state.current.as_deref(), Some("default"));
        assert_eq!(e.state.choices[0].rank, None);
        assert!(e.state.choices[1].rank > e.state.choices[2].rank);
        assert_eq!(e.state.choices[3].rank, None);
        e.update(&json!([]));
        assert!(!e.state.supported);
        assert!(e.state.current.is_none());
    }

    #[test]
    fn maps_session_updates() {
        let e = update_event(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" },
        }));
        assert!(matches!(e, Some(Event::Text(_, t)) if t == "hi"));
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
