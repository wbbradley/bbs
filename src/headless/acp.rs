//! A small Agent Client Protocol client over a child process's stdio.
//!
//! It implements only what headless needs: initialize, new/resume/load a
//! session, set session config options, prompt, and cancel. Permission requests
//! are approved. Session updates become [`Event`]s; anything unrecognized is
//! reported by kind rather than rejected, so harness extensions do not break it.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use super::settings::HarnessConfig;

const PROTOCOL_VERSION: u64 = 1;
/// JSON-RPC error code ACP uses when the agent needs `authenticate` first.
const AUTH_REQUIRED: i64 = -32000;
/// Longest tool input kept in an event.
const MAX_INPUT_CHARS: usize = 2000;

/// What the harness reported during a session.
#[derive(Debug)]
pub enum Event {
    Text(String),
    Thought(String),
    /// A tool call started or changed: id, and the fields worth logging.
    Tool(String, Value),
    /// A permission request and the option chosen for it.
    Permission(Value),
    /// A session update this client does not interpret, by kind.
    Other(String),
    /// The harness's stdout closed.
    Exited,
}

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

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;

struct Inner {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// While a loaded session replays its history, updates are not events.
    replaying: AtomicBool,
}

/// One running ACP agent and the session it serves.
#[derive(Clone)]
pub struct Acp {
    inner: Arc<Inner>,
    session_id: String,
    pub session_resumed: bool,
}

/// What the agent advertised at `initialize`.
struct AgentCaps {
    load: bool,
    resume: bool,
    auth_methods: Vec<String>,
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
    ) -> Result<(Acp, Child, mpsc::UnboundedReceiver<Event>, Vec<String>)> {
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr)
            .with_context(|| format!("opening {}", stderr.display()))?;
        let mut child = Command::new(&config.command[0])
            .args(&config.command[1..])
            .envs(&config.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting harness {:?}", config.command))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            pending: Default::default(),
            next_id: AtomicU64::new(1),
            replaying: AtomicBool::new(false),
        });
        let (events, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(inner.clone(), BufReader::new(stdout), events));

        let mut acp = Acp {
            inner,
            session_id: String::new(),
            session_resumed: false,
        };
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
                acp.inner.replaying.store(true, Ordering::SeqCst);
                let result = acp.request(method, params).await;
                acp.inner.replaying.store(false, Ordering::SeqCst);
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
                let result = match acp.call("session/new", params.clone()).await {
                    Err(err) if err.code == AUTH_REQUIRED && !caps.auth_methods.is_empty() => {
                        acp.request("authenticate", json!({ "methodId": caps.auth_methods[0] }))
                            .await?;
                        acp.request("session/new", params).await?
                    }
                    other => other.map_err(|e| anyhow!("session/new: {}", e.message))?,
                };
                acp.session_id = result["sessionId"]
                    .as_str()
                    .context("session/new returned no sessionId")?
                    .to_string();
                result
            }
        };
        let warnings = acp.configure(&result, config).await?;
        Ok((acp, child, rx, warnings))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// ACP has no way to add input to a running turn.
    pub fn can_steer(&self) -> bool {
        false
    }

    async fn initialize(&self) -> Result<AgentCaps> {
        let result = self
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
            auth_methods: result["authMethods"]
                .as_array()
                .map(|m| {
                    m.iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
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
                self.request(
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
        self.request(
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
        self.notify("session/cancel", json!({ "sessionId": self.session_id }))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.call(method, params)
            .await
            .map_err(|e| anyhow!("{method}: {} (code {})", e.message, e.code))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = send(&self.inner, &msg).await {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(RpcError {
                code: 0,
                message: format!("{err:#}"),
            });
        }
        rx.await.unwrap_or_else(|_| {
            Err(RpcError {
                code: 0,
                message: "the harness exited".into(),
            })
        })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        send(
            &self.inner,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
        .await
    }
}

async fn send(inner: &Inner, msg: &Value) -> Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let mut stdin = inner.stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_loop(
    inner: Arc<Inner>,
    mut stdout: BufReader<tokio::process::ChildStdout>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("harness wrote a non-JSON line");
            continue;
        };
        let method = msg["method"].as_str();
        match (method, msg.get("id")) {
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let Some(tx) = inner.pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let result = match msg.get("error") {
                    Some(err) => Err(RpcError {
                        code: err["code"].as_i64().unwrap_or(0),
                        message: err["message"].as_str().unwrap_or("error").to_string(),
                    }),
                    None => Ok(msg["result"].clone()),
                };
                let _ = tx.send(result);
            }
            (Some(method), Some(id)) => {
                let reply = answer(method, &msg["params"], &events);
                let response = match reply {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(message) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": message },
                    }),
                };
                if send(&inner, &response).await.is_err() {
                    break;
                }
            }
            (Some("session/update"), None) => {
                if !inner.replaying.load(Ordering::SeqCst)
                    && let Some(event) = update_event(&msg["params"]["update"])
                {
                    let _ = events.send(event);
                }
            }
            (Some(other), None) => {
                let _ = events.send(Event::Other(other.to_string()));
            }
            (None, None) => {}
        }
    }
    for (_, tx) in inner.pending.lock().unwrap().drain() {
        let _ = tx.send(Err(RpcError {
            code: 0,
            message: "the harness exited".into(),
        }));
    }
    let _ = events.send(Event::Exited);
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
                let mut s = input.to_string();
                if s.chars().count() > MAX_INPUT_CHARS {
                    s = s.chars().take(MAX_INPUT_CHARS).collect();
                    s.push('…');
                }
                fields.insert("input".into(), s.into());
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
