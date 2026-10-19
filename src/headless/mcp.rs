//! The agent's BBS tools, and `bbs headless mcp`: a stdio MCP server that
//! exposes them to ACP harnesses.
//!
//! The harness starts it; the agent's token arrives in `BBS_HEADLESS_TOKEN`, so
//! it never appears in a process listing. stdout carries only protocol lines.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::app::{
    format_agents, format_groups, format_messages, parse_time, timer_line, timer_schedule,
};
use crate::backend::Backend;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

pub const TOKEN_ENV: &str = "BBS_HEADLESS_TOKEN";
/// Offered when the client asks for none.
const DEFAULT_PROTOCOL: &str = "2025-06-18";
/// History returned when the agent gives no limit: the newest messages...
const DEFAULT_HISTORY_LIMIT: usize = 50;
/// ...from this far back, when it gives no start.
const DEFAULT_HISTORY_SINCE: &str = "7d";

pub async fn run(dir: &Path, local: bool, auto_owner_reply: bool) -> Result<()> {
    if local && !dir.is_absolute() {
        bail!("--local-state-dir must be an absolute path");
    }
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .with_context(|| format!("{TOKEN_ENV} is not set"))?;
    let mut tools = Tools::new(dir, token);
    tools.local = local;
    tools.auto_owner_reply = auto_owner_reply;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notifications need no reply
        };
        let reply = match tools.handle(&msg).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("{err:#}") },
            }),
        };
        let mut out = reply.to_string();
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }
}

/// The agent's BBS tools, acting with its token.
pub struct Tools {
    dir: std::path::PathBuf,
    token: String,
    backend: Option<Backend>,
    local: bool,
    auto_owner_reply: bool,
}

impl Tools {
    pub fn new(dir: &Path, token: String) -> Tools {
        Tools {
            dir: dir.to_path_buf(),
            token,
            backend: None,
            local: false,
            auto_owner_reply: false,
        }
    }

    pub fn auto_owner_reply(mut self) -> Self {
        self.auto_owner_reply = true;
        self
    }

    async fn handle(&mut self, msg: &Value) -> Result<Value> {
        let params = &msg["params"];
        match msg["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(DEFAULT_PROTOCOL),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "bbs", "version": env!("CARGO_PKG_VERSION") },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_list() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = &params["arguments"];
                // Tool failures are results the agent can read, not protocol errors.
                Ok(match self.call(name, args).await {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                    Err(err) => json!({
                        "content": [{ "type": "text", "text": format!("{err:#}") }],
                        "isError": true,
                    }),
                })
            }
            other => bail!("unsupported method {other}"),
        }
    }

    async fn backend(&mut self) -> Result<&mut Backend> {
        if self.backend.is_none() {
            let caller = Caller {
                token: Some(self.token.clone()),
                ..Default::default()
            };
            let config = if self.local {
                Config::default()
            } else {
                Config::load()?
            };
            self.backend = Some(Backend::open(&config, &self.dir, caller).await?);
        }
        Ok(self.backend.as_mut().unwrap())
    }

    /// Run tool `name`; an error is a message for the agent.
    pub async fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let result = self.call_once(name, args).await;
        if result.is_err() {
            // Reconnect on the next call in case the connection was the problem.
            self.backend = None;
        }
        result
    }

    async fn call_once(&mut self, name: &str, args: &Value) -> Result<String> {
        let arg = |key: &str| -> Result<String> {
            args[key]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("missing string argument {key:?}"))
        };
        let guard = self.auto_owner_reply;
        let b = self.backend().await?;
        if name == "bbs_send" && guard {
            let to = arg("to")?;
            let me = b.whoami().await?;
            let owner = me
                .owner
                .context("automatic owner delivery has no registered owner")?;
            if to.trim().eq_ignore_ascii_case(&owner) {
                bail!(
                    "Nothing was sent. Your ordinary replies are delivered to {owner} automatically. Write your next ordinary reply instead of calling bbs_send to your owner."
                );
            }
        }
        Ok(match name {
            "bbs_send" => {
                let r = b
                    .send(proto::SendRequest {
                        to: arg("to")?,
                        body: arg("body")?,
                        control: None,
                    })
                    .await?;
                match r.recipients {
                    Some(n) => format!("Sent to {} ({n} recipients).", r.to),
                    None => format!("Sent to {}.", r.to),
                }
            }
            "bbs_agents" => format_agents(
                &b.list_agents(proto::ListAgentsRequest { all: false })
                    .await?,
            ),
            "bbs_status" => {
                b.set_status(proto::SetStatusRequest { text: arg("text")? })
                    .await?;
                "Status updated.".to_string()
            }
            "bbs_history" => {
                let since = args["since"].as_str().unwrap_or(DEFAULT_HISTORY_SINCE);
                let limit = match args["limit"].as_u64() {
                    Some(n) => usize::try_from(n).context("limit is too large")?,
                    None => DEFAULT_HISTORY_LIMIT,
                };
                // The server returns the oldest first; keep the newest `limit`.
                let r = b
                    .read(proto::ReadRequest {
                        limit: None,
                        archived: true,
                        since: Some(parse_time("since", since, Utc::now())?),
                    })
                    .await?;
                let skip = r.messages.len().saturating_sub(limit);
                let text = format_messages(&r.messages[skip..], false)?;
                if text.is_empty() {
                    "No messages.".to_string()
                } else {
                    text
                }
            }
            "bbs_group_list" => format_groups(&b.group_list().await?.groups, false)?,
            "bbs_group_join" => {
                let r = b
                    .group_join(proto::GroupRequest {
                        group: arg("group")?,
                    })
                    .await?;
                if r.joined {
                    format!("Joined @{}.", r.group)
                } else {
                    format!("Already in @{}.", r.group)
                }
            }
            "bbs_group_leave" => {
                let r = b
                    .group_leave(proto::GroupRequest {
                        group: arg("group")?,
                    })
                    .await?;
                format!("Left @{}.", r.group)
            }
            "bbs_timer_add" => {
                let t = b
                    .timer_add(proto::TimerAddRequest {
                        agent: arg("agent")?,
                        when: arg("when")?,
                        body: arg("text")?,
                    })
                    .await?;
                format!("Timer {} for {}: {}.", t.id, t.agent, timer_schedule(&t))
            }
            "bbs_timer_list" => {
                let agent = args["agent"].as_str().map(str::to_string);
                let r = b.timer_list(proto::TimerListRequest { agent }).await?;
                if r.timers.is_empty() {
                    "No timers.".to_string()
                } else {
                    r.timers.iter().map(timer_line).collect()
                }
            }
            "bbs_timer_rm" => {
                let id = args["id"]
                    .as_i64()
                    .context("missing integer argument \"id\"")?;
                let t = b.timer_remove(proto::TimerRemoveRequest { id }).await?;
                format!("Removed timer {} for {}.", t.id, t.agent)
            }
            other => bail!("unknown tool {other}"),
        })
    }
}

pub fn tool_list() -> Value {
    let object = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    let string = |description: &str| json!({ "type": "string", "description": description });
    json!([
        {
            "name": "bbs_send",
            "description": "Send a BBS message to an agent or a human, or to @group to broadcast to a group where you have write access (marked [w] in bbs_group_list; only a human can grant it). This is the only way your words reach anyone.",
            "inputSchema": object(json!({
                "to": string("Agent or human name, or @group"),
                "body": string("Message text (at most 64 KiB)"),
            }), &["to", "body"]),
        },
        {
            "name": "bbs_agents",
            "description": "List the agents in your groups, with their status notes and last activity.",
            "inputSchema": object(json!({}), &[]),
        },
        {
            "name": "bbs_status",
            "description": "Set your status note: one line on what you are working on, shown to others in bbs_agents.",
            "inputSchema": object(json!({ "text": string("Status text") }), &["text"]),
        },
        {
            "name": "bbs_history",
            "description": "Reread messages you already received: the newest ones (default 50 from the last 7 days), oldest first.",
            "inputSchema": object(json!({
                "since": string("Only messages sent at or after this time: RFC3339, or a duration like 2h or 7d (default 7d)"),
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of messages (default 50)" },
            }), &[]),
        },
        {
            "name": "bbs_group_list",
            "description": "List groups, their members, and what each group is for.",
            "inputSchema": object(json!({}), &[]),
        },
        {
            "name": "bbs_group_join",
            "description": "Join a group. Members can message each other and receive the group's broadcasts.",
            "inputSchema": object(json!({ "group": string("Group name, with or without @") }), &["group"]),
        },
        {
            "name": "bbs_group_leave",
            "description": "Leave a group.",
            "inputSchema": object(json!({ "group": string("Group name, with or without @") }), &["group"]),
        },
        {
            "name": "bbs_timer_add",
            "description": "Set a timer: when it comes due, the BBS sends the text to the agent (yourself, or an agent in one of your groups) as a message from you, marked (timer N). Use it for reminders and recurring check-ins.",
            "inputSchema": object(json!({
                "agent": string("Agent name; your own name for a reminder to yourself"),
                "when": string("\"in 30m\" or \"at <RFC3339 time>\" (once), \"every 1h\", or a 5-field cron expression in UTC such as \"0 9 * * 1-5\""),
                "text": string("Message text the timer sends"),
            }), &["agent", "when", "text"]),
        },
        {
            "name": "bbs_timer_list",
            "description": "List timers, soonest first: your own, or another agent's.",
            "inputSchema": object(json!({ "agent": string("Agent name (default: you)") }), &[]),
        },
        {
            "name": "bbs_timer_rm",
            "description": "Remove a timer by its id, as shown by bbs_timer_list.",
            "inputSchema": object(json!({ "id": { "type": "integer", "description": "Timer id" } }), &["id"]),
        },
    ])
}
