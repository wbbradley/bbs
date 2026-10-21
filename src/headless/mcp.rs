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

    /// A separate connection with the same identity and tool policy.
    pub fn independent(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            token: self.token.clone(),
            backend: None,
            local: self.local,
            auto_owner_reply: self.auto_owner_reply,
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
            "bbs_search" => {
                let request = search_request(args)?;
                let since = request.since.clone();
                let until = request.until.clone();
                let response = b.search(request).await?;
                format_search(response, since, until)?
            }
            "bbs_blockers" => {
                let response = b
                    .list_blockers(crate::blockers::list_request(args)?)
                    .await?;
                crate::blockers::list(&response).to_string()
            }
            "bbs_blocker_get" => {
                let response = b.get_blocker(crate::blockers::get_request(args)?).await?;
                crate::blockers::get(&response).to_string()
            }
            "bbs_blocker_put" => {
                let request = crate::blockers::put_request(args)?;
                let key = request.request_key.clone();
                let response = b.put_blocker(request).await?;
                crate::blockers::mutation(&response, &key).to_string()
            }
            "bbs_blocker_cancel" => {
                let request = crate::blockers::cancel_request(args)?;
                let key = request.request_key.clone();
                let response = b.cancel_blocker(request).await?;
                crate::blockers::mutation(&response, &key).to_string()
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

fn search_request(args: &Value) -> Result<proto::SearchRequest> {
    let query = args["query"]
        .as_str()
        .context(r#"missing string argument "query""#)?;
    let string = |key: &str| -> Result<Option<String>> {
        args.get(key)
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{key} must be a string"))
            })
            .transpose()
    };
    let integer = |key: &str| -> Result<Option<u64>> {
        args.get(key)
            .map(|v| {
                v.as_u64()
                    .with_context(|| format!("{key} must be a positive integer"))
            })
            .transpose()
    };
    let case_sensitive = args
        .get("case_sensitive")
        .map(|v| v.as_bool().context("case_sensitive must be a boolean"))
        .transpose()?
        .unwrap_or(false);
    let now = Utc::now();
    Ok(proto::SearchRequest {
        query: query.to_owned(),
        case_sensitive,
        limit: integer("limit")?
            .map(u32::try_from)
            .transpose()
            .context("limit is too large")?,
        before_id: integer("before_id")?
            .map(i64::try_from)
            .transpose()
            .context("before_id is too large")?,
        from: string("from")?,
        with: string("with")?,
        group: string("group")?,
        since: string("since")?
            .map(|s| parse_time("since", &s, now))
            .transpose()?,
        until: string("until")?
            .map(|s| parse_time("until", &s, now))
            .transpose()?,
    })
}

fn format_search(
    response: proto::SearchResponse,
    since: Option<String>,
    until: Option<String>,
) -> Result<String> {
    let hits = response.hits.into_iter().map(|hit| {
        let m = hit.message.context("server returned a search hit without a message")?;
        Ok(json!({
            "id": m.id, "created_at": m.created_at,
            "from": { "kind": if m.from_kind == proto::Kind::Agent as i32 { "agent" } else { "human" },
                "name": m.from_name, "owner": m.from_owner },
            "to": { "group": m.to_group, "name": m.to_name, "owner": m.to_owner },
            "body": m.body, "truncated": hit.truncated,
            "body_offset_bytes": hit.body_offset_bytes,
            "original_body_bytes": hit.original_body_bytes,
            "match_offset_bytes": hit.match_offset_bytes,
            "match_length_bytes": hit.match_length_bytes,
        }))
    }).collect::<Result<Vec<_>>>()?;
    Ok(serde_json::to_string(
        &json!({ "hits": hits, "has_more": response.has_more,
        "next_before_id": response.next_before_id, "since": since, "until": until }),
    )?)
}

pub fn tool_list() -> Value {
    let object = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    let string = |description: &str| json!({ "type": "string", "description": description });
    json!([
        {
            "name": "bbs_send",
            "description": "Send a BBS message to an agent or a human, or to @group to broadcast to a group where you have write access (marked [w] in bbs_group_list; only a human can grant it). Use blocker tools for structured group help requests.",
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
            "name": "bbs_search",
            "description": "Search literal substrings in ALL ordinary BBS message bodies, including other agents' direct messages and groups. Read-only; does not mark mail read. Unicode case-insensitive by default, accents significant. Newest message IDs first. Returns JSON hits with IDs, time, sender, destination and original body; oversized bodies are marked excerpts. At most 32 KiB of body text per call. For older results repeat the same query and filters with next_before_id as before_id; reuse the absolute since/until echoed in the response instead of reusing relative durations.",
            "inputSchema": object(json!({
                "query": { "type": "string", "minLength": 1, "description": "Literal substring, including whitespace and punctuation; at most 4096 UTF-8 bytes. No regex or wildcard syntax." },
                "case_sensitive": { "type": "boolean", "default": false },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
                "before_id": { "type": "integer", "minimum": 1, "description": "Exclusive message-ID cursor from the previous response's next_before_id." },
                "from": string("Only messages sent by this agent or human name; retired agents included."),
                "with": string("Only messages sent by or directly to this agent or human; not broadcasts delivered to it."),
                "group": string("Only broadcasts to this group name, with or without @; archived groups included."),
                "since": string("Inclusive start time: RFC3339 or a duration like 7d."),
                "until": string("Exclusive end time: RFC3339 or a duration like 1h."),
            }), &["query"]),
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
            "name": "bbs_blockers",
            "description": "List group blockers, oldest first. Authenticated reads cover the whole board. Optional group OR live agent scope; agent means all its groups. Returns current versions, exact active_count and pagination. Requires blockers.v1; never reads or sends mail.",
            "inputSchema": object(json!({
                "group": string("Group name, with or without @; mutually exclusive with agent."),
                "agent": string("Live agent name; all current groups. Mutually exclusive with group."),
                "include_closed": { "type":"boolean", "description":"Include answered/cancelled chain heads and archived groups; default false." },
                "limit": { "type":"integer", "minimum":1, "maximum":100, "description":"Page size; default 50." },
                "after_root_id": { "type":"integer", "minimum":1, "description":"Exclusive next_after_root_id from the previous page; reuse the same scope." },
            }), &[]),
        },
        {
            "name": "bbs_blocker_get",
            "description": "Read one immutable blocker version, the current version, and linked history (newest first). Reads are board-wide; requires blockers.v1.",
            "inputSchema": object(json!({
                "id": { "type":"integer", "minimum":1, "description":"Exact blocker version ID." },
                "limit": { "type":"integer", "minimum":1, "maximum":100, "description":"History page size; default 20." },
                "before_id": { "type":"integer", "minimum":1, "description":"Exclusive next_before_id from the previous history page." },
            }), &["id"]),
        },
        {
            "name": "bbs_blocker_put",
            "description": "Create a group blocker or replace a chain's current version with a new linked immutable version. You must currently belong to the group; no broadcast write grant needed. Replacement supersedes an open version, preserves history and queue position. Does not send ordinary mail. Requires blockers.v1. Reuse request_key with the same payload after an uncertain result; never generate a new key just to retry.",
            "inputSchema": object(json!({
                "group": string("Group name, with or without @."),
                "body": string("One nonblank plaintext description, at most 8192 UTF-8 bytes; original whitespace is preserved."),
                "replaces": { "type":"integer", "minimum":1, "description":"Optional current predecessor version, including answered/cancelled heads. Must be in the same group." },
                "request_key": string("Unique key for this intent, 1..128 UTF-8 bytes. Reuse only with exactly the same operation and payload."),
            }), &["group", "body", "request_key"]),
        },
        {
            "name": "bbs_blocker_cancel",
            "description": "Cancel an exact open blocker version in a group you currently belong to. Keeps immutable history and sends no mail. Requires blockers.v1. Retry an uncertain result with the same request_key and ID.",
            "inputSchema": object(json!({
                "id": { "type":"integer", "minimum":1, "description":"Exact open version ID; stale versions are refused." },
                "request_key": string("Unique 1..128 byte intent key; reuse for the same retry."),
            }), &["id", "request_key"]),
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

#[cfg(test)]
mod search_tests {
    use super::*;
    #[test]
    fn search_tool_preserves_literal_input_filters_and_cursor() {
        let args = json!({"query":"  É.*  ","case_sensitive":true,"limit":100,"before_id":42,
            "from":"Ada","with":"Mateo","group":"@crew","since":"2020-01-01T00:00:00Z","until":"2020-01-02T00:00:00Z"});
        let r = search_request(&args).unwrap();
        assert_eq!(r.query, "  É.*  ");
        assert!(r.case_sensitive);
        assert_eq!(r.limit, Some(100));
        assert_eq!(r.before_id, Some(42));
        assert_eq!(r.from.as_deref(), Some("Ada"));
        assert_eq!(r.with.as_deref(), Some("Mateo"));
        assert_eq!(r.group.as_deref(), Some("@crew"));
        assert_eq!(r.since.as_deref(), Some("2020-01-01T00:00:00.000000Z"));
        let r = search_request(&json!({"query":"x","since":"7d"})).unwrap();
        assert!(r.limit.is_none() && !r.case_sensitive && r.before_id.is_none());
        assert!(r.since.is_some());
        for bad in [
            json!({}),
            json!({"query":1}),
            json!({"query":"x","limit":-1}),
            json!({"query":"x","limit":"20"}),
            json!({"query":"x","case_sensitive":"false"}),
            json!({"query":"x","before_id":u64::MAX}),
            json!({"query":"x","from":1}),
        ] {
            assert!(search_request(&bad).is_err(), "{bad}");
        }
    }
}
