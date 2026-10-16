//! `bbs headless mcp`: a stdio MCP server exposing the agent's BBS tools.
//!
//! The harness starts it; the agent's token arrives in `BBS_HEADLESS_TOKEN`, so
//! it never appears in a process listing. stdout carries only protocol lines.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::app::{format_agents, format_groups, format_messages, parse_time};
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

pub async fn run(dir: &Path) -> Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .with_context(|| format!("{TOKEN_ENV} is not set"))?;
    let mut tools = Tools {
        dir: dir.to_path_buf(),
        token,
        backend: None,
    };
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

struct Tools {
    dir: std::path::PathBuf,
    token: String,
    backend: Option<Backend>,
}

impl Tools {
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
            self.backend = Some(Backend::open(&Config::load()?, &self.dir, caller).await?);
        }
        Ok(self.backend.as_mut().unwrap())
    }

    async fn call(&mut self, name: &str, args: &Value) -> Result<String> {
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
        let b = self.backend().await?;
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
            other => bail!("unknown tool {other}"),
        })
    }
}

fn tool_list() -> Value {
    let object = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    let string = |description: &str| json!({ "type": "string", "description": description });
    json!([
        {
            "name": "bbs_send",
            "description": "Send a BBS message to an agent, or to @group to broadcast to a group where you have write access (marked [w] in bbs_group_list; only a human can grant it). This is the only way your words reach anyone.",
            "inputSchema": object(json!({
                "to": string("Agent name, or @group"),
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
    ])
}
