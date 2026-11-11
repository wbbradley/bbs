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
    format_agents, format_directory, format_groups, format_messages, parse_time, remote_directory,
    timer_line, timer_schedule,
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
/// Entities `bbs_directory` shows by default, and at most.
const DEFAULT_DIRECTORY_LIMIT: usize = 50;
const MAX_DIRECTORY_LIMIT: u64 = 200;

pub async fn run(
    dir: &Path,
    local: bool,
    auto_owner_reply: bool,
    goal: Option<GoalTool>,
) -> Result<()> {
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
    tools.goal = goal;
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

/// Where `bbs_goal_update` finds the runner's goal and leaves updates for it.
#[derive(Clone)]
pub struct GoalTool {
    /// The agent's headless state directory.
    pub dir: std::path::PathBuf,
    pub max_wait: std::time::Duration,
}

/// The agent's BBS tools, acting with its token.
pub struct Tools {
    dir: std::path::PathBuf,
    token: String,
    backend: Option<Backend>,
    local: bool,
    auto_owner_reply: bool,
    goal: Option<GoalTool>,
}

impl Tools {
    pub fn new(dir: &Path, token: String) -> Tools {
        Tools {
            dir: dir.to_path_buf(),
            token,
            backend: None,
            local: false,
            auto_owner_reply: false,
            goal: None,
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
            goal: self.goal.clone(),
        }
    }

    pub fn auto_owner_reply(mut self) -> Self {
        self.auto_owner_reply = true;
        self
    }

    pub fn goal(mut self, goal: GoalTool) -> Self {
        self.goal = Some(goal);
        self
    }

    /// `bbs_goal_update`: check the update against the runner's goal and
    /// leave it for the runner, which applies it when the turn ends.
    async fn goal_update(&mut self, args: &Value) -> Result<String> {
        let Some(tool) = self.goal.clone() else {
            bail!("Goals are only available to agents run by `bbs headless run`.");
        };
        let state = super::state::AgentDir {
            dir: tool.dir.clone(),
        }
        .load()?;
        let (mut update, reply) =
            super::goal::tool_update(state.goal.as_ref(), args, tool.max_wait)?;
        if let super::goal::UpdateKind::Blocked {
            blocker: Some(id), ..
        } = &mut update.kind
        {
            *id = self.own_open_blocker(*id).await?;
        }
        super::goal::append_update(&tool.dir, &update)?;
        Ok(reply)
    }

    /// `bbs_goal_propose`: put a blocker proposing a goal, which the server
    /// sets if the agent's owner approves it.
    async fn goal_propose(&mut self, args: &Value) -> Result<String> {
        if self.goal.is_none() {
            bail!("Goals are only available to agents run by `bbs headless run`.");
        }
        let request = crate::blockers::goal_request(args)?;
        let key = request.request_key.clone();
        let response = self.backend().await?.put_blocker(request).await?;
        Ok(crate::blockers::mutation(&response, &key).to_string())
    }

    /// `bbs_goal_get`: the runner's goal as it is now, with the instructions
    /// that short goal prompts leave out.
    fn goal_get(&self) -> Result<String> {
        let Some(tool) = &self.goal else {
            bail!("Goals are only available to agents run by `bbs headless run`.");
        };
        let state = super::state::AgentDir {
            dir: tool.dir.clone(),
        }
        .load()?;
        Ok(match state.goal {
            Some(goal) => goal.describe(),
            None => "You have no goal.".to_string(),
        })
    }

    /// Check that blocker `id` is the agent's own and still open; its root,
    /// which stays the same when the blocker is replaced.
    async fn own_open_blocker(&mut self, id: i64) -> Result<i64> {
        let b = self.backend().await?;
        let me = b.whoami().await?;
        let found = b
            .get_blocker(proto::GetBlockerRequest {
                id,
                limit: Some(1),
                ..Default::default()
            })
            .await?;
        let current = found
            .current
            .context("the server returned no current blocker")?;
        if current.creator.as_ref().map(|c| c.id) != Some(me.id) {
            bail!("Blocker #{id} is not yours: block the goal on a blocker you put.");
        }
        if current.status() != proto::BlockerStatus::BlockerOpen {
            bail!(
                "Blocker #{id} is already closed: read its outcome with bbs_blocker_get, or put \
                 a new blocker."
            );
        }
        Ok(current.root_id)
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
        if name == "bbs_goal_get" {
            return self.goal_get();
        }
        if name == "bbs_goal_update" {
            return self.goal_update(args).await;
        }
        if name == "bbs_goal_propose" {
            return self.goal_propose(args).await;
        }
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
                        reply_to: None,
                        not_to_owner: false,
                    })
                    .await?;
                let mut out = match r.recipients {
                    Some(n) => format!("Sent to {} ({n} recipients).", r.to),
                    None => format!("Sent to {}.", r.to),
                };
                if !r.skipped.is_empty() {
                    out.push_str(&format!(
                        " Not sent to members in focus mode: {}.",
                        r.skipped.join(", ")
                    ));
                }
                out
            }
            "bbs_reply" => {
                let id = message_id(&args["message_id"])?;
                let mut req = crate::app::reply_request(id, arg("body")?);
                // Like bbs_send's check above, but the server knows who the
                // reply goes to.
                req.not_to_owner = guard;
                let r = b.send(req).await?;
                format!("Replied to {}.", r.to)
            }
            "bbs_agents" => format_agents(
                &b.list_agents(proto::ListAgentsRequest { all: false })
                    .await?,
            ),
            "bbs_directory" => {
                let text = |key: &str| args[key].as_str().unwrap_or_default().trim().to_string();
                let req = proto::RemoteDirectoryRequest {
                    namespace: arg("namespace")?,
                    alias: text("alias"),
                    entity_id: text("id"),
                    ..Default::default()
                };
                let limit = match args["limit"].as_u64() {
                    None => DEFAULT_DIRECTORY_LIMIT,
                    Some(n @ 1..=MAX_DIRECTORY_LIMIT) => n as usize,
                    Some(_) => bail!("limit must be 1 to {MAX_DIRECTORY_LIMIT}"),
                };
                let (entities, more) = remote_directory(b, req.clone(), limit).await?;
                let mut out = format_directory(&req, &entities);
                if more {
                    out.push_str(&format!(
                        "(more exist; only the first {limit} are shown: narrow the lookup with \
                         alias or id)\n"
                    ));
                }
                out
            }
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

/// A message id given as a number or a numeric string.
fn message_id(v: &Value) -> Result<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .context(r#"missing integer argument "message_id""#)
}

fn format_search(
    response: proto::SearchResponse,
    since: Option<String>,
    until: Option<String>,
) -> Result<String> {
    let hits = response
        .hits
        .into_iter()
        .map(|hit| {
            let m = hit
                .message
                .context("server returned a search hit without a message")?;
            Ok(json!({
                "id": m.id, "created_at": m.created_at,
                "from": { "kind": match m.from_kind() {
                        proto::Kind::Agent => "agent",
                        proto::Kind::Remote => "remote",
                        _ => "human",
                    },
                    "name": m.from_name, "owner": m.from_owner },
                "original_from": m.original_from, "reported": m.reported == Some(true),
                "notice": m.group_notice == Some(true), "in_reply_to": m.in_reply_to,
                "to": { "group": m.to_group, "name": m.to_name, "owner": m.to_owner },
                "body": m.body, "truncated": hit.truncated,
                "body_offset_bytes": hit.body_offset_bytes,
                "original_body_bytes": hit.original_body_bytes,
                "match_offset_bytes": hit.match_offset_bytes,
                "match_length_bytes": hit.match_length_bytes,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
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
                "to": string("Agent or human name, @group, or a canonical address id:ENTITY@NAMESPACE: an agent or human here (bbs_agents lists agents' addresses) or an entity in another namespace (see bbs_directory)"),
                "body": string("Message text (at most 64 KiB)"),
            }), &["to", "body"]),
        },
        {
            "name": "bbs_reply",
            "description": "Reply to a message you received, by its id (shown as (id N) in mail headers and bbs_history). The reply goes to the message's exact sender, or for a copy from another installation's group (from X via GROUP) to its original sender X, not the group, and is marked as answering that message. A group notice has no sender to reply to.",
            "inputSchema": object(json!({
                "message_id": json!({ "type": "integer", "description": "The id of the message to reply to" }),
                "body": string("Message text (at most 64 KiB)"),
            }), &["message_id", "body"]),
        },
        {
            "name": "bbs_agents",
            "description": "List the agents in your groups, with their canonical addresses (id:ENTITY@NAMESPACE, including your own), status notes and last activity.",
            "inputSchema": object(json!({}), &[]),
        },
        {
            "name": "bbs_directory",
            "description": "List the entities another namespace (such as hub) publishes, or look one up by alias or ID, through this BBS's Hub integration. Each line starts with the id:ENTITY@NAMESPACE that bbs_send accepts as its recipient. An unreachable or unknown namespace is an error, never an empty list.",
            "inputSchema": object(json!({
                "namespace": string("The other installation's namespace, e.g. hub"),
                "alias": string("Look up the entity with this alias; mutually exclusive with id."),
                "id": string("Look up the entity with this exact entity ID; mutually exclusive with alias."),
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Show at most this many entities; default 50." },
            }), &["namespace"]),
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
                "from": string("Only messages sent by this agent or human name; retired agents included. A copy from another installation's group matches the group, not its original sender."),
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
                "body": string("One nonblank Markdown description, at most 2 KiB (2048 UTF-8 bytes), including formatting, stored as written. Separate paragraphs with a blank line: a single newline joins lines as in any Markdown."),
                "replaces": { "type":"integer", "minimum":1, "description":"Optional current predecessor version, including answered/cancelled heads. Must be in the same group." },
                "request_key": string("Unique key for this intent, 1..128 UTF-8 bytes. Reuse only with exactly the same operation and payload."),
                "choices": {
                    "type": "object",
                    "description": "Optional response configuration, at most 1 KiB (1024 UTF-8 bytes of canonical JSON including mode, options and defaulted select_many bounds). Options appear as buttons (select_one) or checkboxes (select_many). The human can always answer with free text; answer mail lists each pick as `Selected: <label> [<id>]`, then any note. Requires blocker-choices.v1; limits are checked before sending and by the server.",
                    "properties": {
                        "mode": { "type": "string", "enum": ["select_one", "select_many"] },
                        "options": {
                            "type": "array", "minItems": 2, "maxItems": 8,
                            "description": "Shown in this order.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string", "pattern": "^[a-z0-9_-]{1,32}$", "description": "Unique stable ID echoed back in the answer." },
                                    "label": string("One nonblank line, at most 120 UTF-8 bytes."),
                                    "detail": string("Optional explanation, at most 500 UTF-8 bytes."),
                                },
                                "required": ["id", "label"],
                                "additionalProperties": false,
                            },
                        },
                        "min": { "type": "integer", "minimum": 1, "description": "select_many only: fewest picks; default 1." },
                        "max": { "type": "integer", "minimum": 1, "description": "select_many only: most picks; default all options." },
                    },
                    "required": ["mode", "options"],
                    "additionalProperties": false,
                },
            }), &["group", "body", "request_key"]),
        },
        {
            "name": "bbs_blocker_cancel",
            "description": "Cancel an exact open blocker version in a group you currently belong to. Keeps immutable history; its creator is mailed a notice unless you are the creator. Requires blockers.v1. Retry an uncertain result with the same request_key and ID.",
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
            "name": "bbs_goal_get",
            "description": "Read the goal your runner keeps prompting you toward: its objective, status, turns used and the instructions for reporting progress. Goal prompts after the first are short; use this to reread the objective.",
            "inputSchema": object(json!({}), &[]),
        },
        {
            "name": "bbs_goal_propose",
            "description": "Propose a goal for yourself, or a change to your current one: the only way you can set or change your goal. Puts a blocker in a group you belong to, choices fixed to approve or decline, that only your owner can answer. The server adds the objective and time limit to the blocker's text. If they approve, the goal is set from them, replacing any current goal and its deadline, and your runner prompts you toward it; either way the answer arrives as mail. After an approval, check with bbs_goal_get: a runner with a Codex native goal running refuses the new goal, and then tell your owner. Requires blocker-goals.v1. Reuse request_key with the same payload after an uncertain result.",
            "inputSchema": object(json!({
                "group": string("Group name, with or without @."),
                "objective": string("The goal, 1..640 UTF-8 bytes (at most 1 KiB stored with the choices, after JSON escaping), set verbatim if approved: what to achieve and how to tell it is done. Put context in body."),
                "body": string("Why you propose it, for your owner: one nonblank Markdown description, at most 2 KiB (2048 UTF-8 bytes)."),
                "for": string("Optional time limit counted from approval, such as 8h or 3days (at most 366 days): the goal's deadline."),
                "replaces": { "type":"integer", "minimum":1, "description":"Optional current predecessor blocker version, such as an earlier proposal. Must be in the same group." },
                "request_key": string("Unique key for this intent, 1..128 UTF-8 bytes. Reuse only with exactly the same operation and payload."),
            }), &["group", "objective", "body", "request_key"]),
        },
        {
            "name": "bbs_goal_update",
            "description": "Report progress on the goal your runner keeps prompting you toward: complete (with evidence), blocked (on a blocker you put with bbs_blocker_put; goal prompts resume when it is answered or cancelled), waiting (with a reason and either until or for; you are prompted again then) or active (end a wait or block early, for example when mail you got while waiting, or a result you were watching for, ends it). Mail reaches you while you wait or are blocked, without goal prompts. Takes effect when your turn ends.",
            "inputSchema": object(json!({
                "status": { "type": "string", "enum": ["complete", "blocked", "waiting", "active"], "description": "The goal's new status" },
                "evidence": string("complete: what shows the objective is met, such as checks you ran"),
                "blocker": { "type": "integer", "description": "blocked: the id of your open blocker, from bbs_blocker_put" },
                "reason": string("blocked or waiting: what you need, or what you are waiting for"),
                "until": string("waiting: an RFC 3339 time to be prompted again, such as 2026-11-06T09:00:00Z"),
                "for": string("waiting: how long to wait instead, such as 30m or 2h"),
            }), &["status"]),
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
    fn the_directory_tool_offers_a_bounded_limit() {
        let tools = tool_list();
        let tool = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "bbs_directory")
            .unwrap();
        let limit = &tool["inputSchema"]["properties"]["limit"];
        assert_eq!(limit["maximum"], MAX_DIRECTORY_LIMIT);
        assert_eq!(DEFAULT_DIRECTORY_LIMIT, 50);
    }

    /// The MCP server acts only as the runner's existing agent: it offers no
    /// way to register one.
    #[test]
    fn no_tool_registers_an_agent() {
        for tool in tool_list().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(!name.contains("register"), "{name}");
        }
    }

    #[tokio::test]
    async fn goal_updates_are_left_for_the_runner() {
        let dir = tempfile::tempdir().unwrap();
        let agent = super::super::state::AgentDir {
            dir: dir.path().to_path_buf(),
        };
        let mut tools = Tools::new(dir.path(), "token".into());
        let args = json!({"status": "waiting", "reason": "CI", "for": "30m"});
        // Not run by a runner.
        let err = tools.call("bbs_goal_update", &args).await.unwrap_err();
        assert!(format!("{err:#}").contains("bbs headless run"), "{err:#}");
        let mut tools = tools.goal(GoalTool {
            dir: dir.path().to_path_buf(),
            max_wait: super::super::goal::DEFAULT_MAX_WAIT,
        });
        // No goal yet.
        assert!(tools.call("bbs_goal_update", &args).await.is_err());
        let goal = super::super::goal::Goal::new("Ship".into(), "will".into(), 5);
        agent
            .save(&super::super::state::State {
                goal: Some(goal.clone()),
                ..Default::default()
            })
            .unwrap();
        let reply = tools.call("bbs_goal_update", &args).await.unwrap();
        assert!(reply.starts_with("Waiting until "), "{reply}");
        // bbs_goal_get restates the goal that short prompts leave out.
        let got = tools.call("bbs_goal_get", &json!({})).await.unwrap();
        assert!(got.starts_with("Objective (set by will, "), "{got}");
        assert!(
            got.contains("Ship\n\nStatus: active (turn 0 of 5)"),
            "{got}"
        );
        assert!(got.ends_with("Don't mark the goal complete just to end the loop."));
        let updates = super::super::goal::take_updates(dir.path()).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].goal, goal.set_at);
        assert!(
            tool_list()
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "bbs_goal_update")
        );
        assert!(
            tool_list()
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "bbs_goal_get")
        );
    }
    #[tokio::test]
    async fn blocking_needs_an_open_blocker_of_the_agents_own() {
        use super::super::goal::{Goal, UpdateKind};
        use crate::service::Caller;
        let board = tempfile::tempdir().unwrap();
        let mut db = crate::db::open(&board.path().join("bbs.db")).unwrap();
        crate::db::migrate(&mut db).unwrap();
        drop(db);
        let config = Config::default();
        let open = |caller: Caller| Backend::open(&config, board.path(), caller);
        let mut human = open(Caller {
            human: Some("will".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        human
            .ensure_human(proto::EnsureHumanRequest {
                name: "will".into(),
            })
            .await
            .unwrap();
        human
            .group_register(proto::GroupRegisterRequest {
                group: "crew".into(),
                description: None,
            })
            .await
            .unwrap();
        let mut agents = Vec::new();
        for name in ["Agent", "Other"] {
            let token = human
                .register(proto::RegisterRequest {
                    name: Some(name.into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .token;
            let mut b = open(Caller {
                token: Some(token.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
            b.group_join(proto::GroupRequest {
                group: "crew".into(),
            })
            .await
            .unwrap();
            agents.push((token, b));
        }
        async fn put(
            b: &mut Backend,
            key: &str,
            replaces: Option<i64>,
        ) -> Result<proto::BlockerMutationResponse> {
            b.put_blocker(proto::PutBlockerRequest {
                group: "crew".into(),
                body: "Which branch?".into(),
                request_key: key.into(),
                replaces,
                choices: None,
                goal: None,
            })
            .await
        }
        let mine = put(&mut agents[0].1, "mine", None)
            .await
            .unwrap()
            .blocker
            .unwrap();
        let newer = put(&mut agents[0].1, "newer", Some(mine.id))
            .await
            .unwrap()
            .blocker
            .unwrap();
        let theirs = put(&mut agents[1].1, "theirs", None)
            .await
            .unwrap()
            .blocker
            .unwrap();
        let done = put(&mut agents[0].1, "done", None)
            .await
            .unwrap()
            .blocker
            .unwrap();
        agents[0]
            .1
            .cancel_blocker(proto::CancelBlockerRequest {
                id: done.id,
                request_key: "cancel".into(),
            })
            .await
            .unwrap();

        let state = tempfile::tempdir().unwrap();
        let goal = Goal::new("Ship".into(), "will".into(), 5);
        super::super::state::AgentDir {
            dir: state.path().to_path_buf(),
        }
        .save(&super::super::state::State {
            goal: Some(goal.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut tools = Tools {
            local: true,
            ..Tools::new(board.path(), agents[0].0.clone())
        }
        .goal(GoalTool {
            dir: state.path().to_path_buf(),
            max_wait: super::super::goal::DEFAULT_MAX_WAIT,
        });
        let mut block = async |blocker: Value| {
            let args = json!({"status": "blocked", "reason": "which branch?", "blocker": blocker});
            tools
                .call("bbs_goal_update", &args)
                .await
                .map_err(|e| format!("{e:#}"))
        };
        assert!(
            block(Value::Null)
                .await
                .unwrap_err()
                .contains("bbs_blocker_put")
        );
        assert!(
            block(json!(theirs.id))
                .await
                .unwrap_err()
                .contains("not yours")
        );
        assert!(
            block(json!(done.id))
                .await
                .unwrap_err()
                .contains("already closed")
        );
        assert!(block(json!(9999)).await.is_err(), "no such blocker");
        assert!(
            super::super::goal::take_updates(state.path())
                .unwrap()
                .is_empty()
        );
        // The newest version, recorded by its root.
        let reply = block(json!(newer.id)).await.unwrap();
        assert!(
            reply.starts_with(&format!("Goal blocked on blocker #{}.", newer.id)),
            "{reply}"
        );
        let updates = super::super::goal::take_updates(state.path()).unwrap();
        assert_eq!(
            updates.iter().map(|u| &u.kind).collect::<Vec<_>>(),
            [&UpdateKind::Blocked {
                reason: "which branch?".into(),
                blocker: Some(mine.id),
            }]
        );
        // The old version is closed now: it was replaced.
        assert!(
            block(json!(mine.id)).await.is_ok(),
            "the chain's current version is open"
        );
    }

    #[tokio::test]
    async fn bbs_reply_refuses_the_owner_when_output_already_reaches_them() {
        use crate::service::Caller;
        let board = tempfile::tempdir().unwrap();
        let mut db = crate::db::open(&board.path().join("bbs.db")).unwrap();
        crate::db::migrate(&mut db).unwrap();
        drop(db);
        let config = Config::default();
        let open = |caller: Caller| Backend::open(&config, board.path(), caller);
        let human = |name: &str| Caller {
            human: Some(name.into()),
            ..Default::default()
        };
        let mut will = open(human("will")).await.unwrap();
        for name in ["will", "ada"] {
            will.ensure_human(proto::EnsureHumanRequest { name: name.into() })
                .await
                .unwrap();
        }
        let token = will
            .register(proto::RegisterRequest {
                name: Some("Agent".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .token;
        let mut ada = open(human("ada")).await.unwrap();
        let mut say = async |b: &mut Backend, body: &str| {
            b.send(proto::SendRequest {
                to: "Agent".into(),
                body: body.into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
            .unwrap()
        };
        let from_will = say(&mut will, "from will").await;
        let from_ada = say(&mut ada, "from ada").await;
        let tools = || Tools {
            local: true,
            ..Tools::new(board.path(), token.clone())
        };
        let reply = async |mut tools: Tools, id: Value| {
            tools
                .call("bbs_reply", &json!({"message_id": id, "body": "re"}))
                .await
                .map_err(|e| format!("{e:#}"))
        };
        let err = reply(tools().auto_owner_reply(), json!(from_will))
            .await
            .unwrap_err();
        assert!(err.starts_with("Nothing was sent."), "{err}");
        // Another sender is answered; the id may be a numeric string.
        assert_eq!(
            reply(tools().auto_owner_reply(), json!(from_ada.to_string())).await,
            Ok("Replied to ada.".to_string())
        );
        assert!(reply(tools(), json!("x")).await.is_err());
        // Without automatic owner delivery, the owner is answered like anyone.
        assert_eq!(
            reply(tools(), json!(from_will)).await,
            Ok("Replied to will.".to_string())
        );
    }

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
