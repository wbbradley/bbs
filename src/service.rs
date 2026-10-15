//! Business logic: one function per command over a SQLite connection, taking
//! and returning the protobuf types. Each runs in its own `IMMEDIATE`
//! transaction, generates `now`, and resolves the caller. It knows nothing
//! about sockets, files or output formatting.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use rand::seq::IndexedRandom;
use rusqlite::{Connection, TransactionBehavior};

use crate::auth::{self, AuthenticationError};
use crate::db;
use crate::names::NAMES;
use crate::proto;

const RETIRE_AFTER_DAYS: i64 = 7;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Group descriptions are one line, counted in characters (also a schema CHECK).
const MAX_GROUP_DESCRIPTION_CHARS: usize = 200;
/// `active_at` is refreshed at most this often.
const ACTIVE_AT_DEBOUNCE: chrono::Duration = chrono::Duration::seconds(60);

/// Who is calling: the agent owning `token`, else the human named `human`.
#[derive(Default, Clone)]
pub struct Caller {
    pub token: Option<String>,
    pub human: Option<String>,
    pub human_token: Option<String>,
    /// Set by the server, never accepted from client-supplied metadata.
    pub require_human_token: bool,
}

const NO_CALLER: Caller = Caller {
    token: None,
    human: None,
    human_token: None,
    require_human_token: false,
};

pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Run `f` in its own immediate (write-locking) transaction.
fn with_ctx<T>(
    conn: &mut Connection,
    caller: &Caller,
    f: impl FnOnce(&Ctx) -> Result<T>,
) -> Result<T> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = Utc::now();
    let ctx = Ctx {
        conn: &tx,
        now,
        now_s: fmt_time(now),
        caller,
    };
    let out = f(&ctx)?;
    tx.commit()?;
    Ok(out)
}

struct Ctx<'a> {
    conn: &'a Connection,
    now: DateTime<Utc>,
    now_s: String,
    caller: &'a Caller,
}

impl Ctx<'_> {
    /// Cutoff for `db::touch_*`: only `active_at` values older than this are rewritten.
    fn touch_cutoff(&self) -> String {
        fmt_time(self.now - ACTIVE_AT_DEBOUNCE)
    }

    /// Resolve the caller and record activity.
    fn actor(&self) -> Result<db::Principal> {
        if let Some(token) = &self.caller.token {
            let token = token.trim().to_ascii_lowercase();
            let Some(agent) = db::agent_by_token(self.conn, &token)? else {
                return Err(
                    AuthenticationError("unknown or retired token; run `bbs register`").into(),
                );
            };
            if self.caller.require_human_token
                && (!auth::is_strong_token(&token)
                    || !db::agent_owner_enabled(self.conn, agent.id)?)
            {
                return Err(AuthenticationError(
                    "agent credential is disabled or needs re-registration",
                )
                .into());
            }
            db::touch_agent(self.conn, agent.id, &self.now_s, &self.touch_cutoff())?;
            return Ok(agent);
        }
        if let Some(token) = &self.caller.human_token {
            let human = db::human_by_token_hash(self.conn, &auth::token_hash(token))?
                .ok_or(AuthenticationError("invalid or revoked human token"))?;
            if self
                .caller
                .human
                .as_ref()
                .is_some_and(|name| !name.eq_ignore_ascii_case(&human.name))
            {
                return Err(AuthenticationError(
                    "claimed human does not match the authenticated human",
                )
                .into());
            }
            db::touch_human(self.conn, human.id, &self.now_s, &self.touch_cutoff())?;
            return Ok(human);
        }
        if self.caller.require_human_token {
            return Err(AuthenticationError(
                "human token required; run `bbs init --server <address>`",
            )
            .into());
        }
        let Some(name) = &self.caller.human else {
            bail!("no caller identity; pass --token, or --as <human>");
        };
        let Some(human) = db::find_human(self.conn, name)? else {
            bail!("unknown human {name:?}; run `bbs init` or `bbs human add {name}`");
        };
        db::touch_human(self.conn, human.id, &self.now_s, &self.touch_cutoff())?;
        Ok(human)
    }

    fn agent(&self) -> Result<db::Principal> {
        if self.caller.token.is_none() {
            bail!("this command requires --token");
        }
        self.actor()
    }

    fn human(&self) -> Result<db::Principal> {
        if self.caller.token.is_some() {
            bail!("this command is for humans only");
        }
        self.actor()
    }
}

fn proto_kind(kind: db::Kind) -> proto::Kind {
    match kind {
        db::Kind::Agent => proto::Kind::Agent,
        db::Kind::Human => proto::Kind::Human,
    }
}

fn principal(p: db::Principal) -> proto::Principal {
    proto::Principal {
        kind: proto_kind(p.kind).into(),
        id: p.id,
        name: p.name,
    }
}

/// Direct messages from an inbox carry the reader's name as `to_name`.
fn message(m: db::MessageView, reader: Option<&str>) -> proto::Message {
    let to_name = match (&m.to_group, m.to_name) {
        (None, None) => reader.map(str::to_string),
        (_, to_name) => to_name,
    };
    proto::Message {
        id: m.id,
        created_at: m.created_at,
        from_kind: proto_kind(m.from_kind).into(),
        from_name: m.from_name,
        from_owner: m.from_owner,
        reader_owner: m.reader_owner,
        to_group: m.to_group,
        to_name,
        read_at: m.read_at,
        body: m.body,
    }
}

fn validate_principal_name(name: &str, kind: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        bail!(
            "invalid {kind} name {name:?}: use 1-64 letters, digits, '.', '_' or '-', starting with a letter or digit"
        );
    }
    Ok(())
}

fn validate_group_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c));
    if !ok {
        bail!("invalid group name {name:?}: use lowercase letters, digits, '_' or '-'");
    }
    Ok(())
}

/// Unicode format characters (general category Cf): bidi overrides and
/// isolates, zero-width spaces and joiners, the BOM, and tag characters. They
/// render invisibly and can make a description display differently from its
/// stored text.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// A trimmed, single-line description within the length limit.
fn validate_group_description(description: &str) -> Result<&str> {
    let description = description.trim();
    if description.is_empty() {
        bail!("group description is empty");
    }
    if description
        .chars()
        .any(|c| c.is_control() || is_format_char(c) || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        bail!(
            "group description must be one line without control or invisible formatting characters"
        );
    }
    let chars = description.chars().count();
    if chars > MAX_GROUP_DESCRIPTION_CHARS {
        bail!(
            "group description is {chars} characters; the limit is {MAX_GROUP_DESCRIPTION_CHARS}"
        );
    }
    Ok(description)
}

fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        bail!("message body is empty");
    }
    if body.len() > MAX_BODY_BYTES {
        bail!("message body exceeds {} KiB", MAX_BODY_BYTES / 1024);
    }
    Ok(())
}

fn insert_human(ctx: &Ctx, name: &str) -> Result<()> {
    validate_principal_name(name, "human")?;
    if db::find_human(ctx.conn, name)?.is_some() {
        bail!("human {name:?} already exists");
    }
    if db::live_agent_by_name(ctx.conn, name)?.is_some() {
        bail!("{name:?} is in use by an agent");
    }
    db::insert_human(ctx.conn, name, &ctx.now_s)?;
    tracing::info!(human = name, "added human");
    Ok(())
}

// ---- identities ------------------------------------------------------------

pub fn ensure_human(
    conn: &mut Connection,
    req: proto::EnsureHumanRequest,
) -> Result<proto::EnsureHumanResponse> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let created = db::find_human(ctx.conn, &req.name)?.is_none();
        if created {
            insert_human(ctx, &req.name)?;
        }
        Ok(proto::EnsureHumanResponse { created })
    })
}

pub fn add_human(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AddHumanRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        ctx.human()?;
        insert_human(ctx, &req.name)?;
        Ok(proto::Empty {})
    })
}

fn pick_name(conn: &Connection) -> Result<String> {
    let taken = db::taken_names(conn)?;
    let last_used = db::name_last_used(conn)?;
    let free: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|n| !taken.contains(&n.to_lowercase()))
        .collect();
    let never_used: Vec<&str> = free
        .iter()
        .copied()
        .filter(|n| !last_used.contains_key(&n.to_lowercase()))
        .collect();
    if let Some(name) = never_used.choose(&mut rand::rng()) {
        return Ok(name.to_string());
    }
    // Every free name has been used before: recycle the least recently used one.
    free.iter()
        .min_by_key(|n| last_used.get(&n.to_lowercase()))
        .map(|n| n.to_string())
        .with_context(|| format!("name pool exhausted: all {} names are in use", NAMES.len()))
}

fn new_token(conn: &Connection) -> Result<String> {
    loop {
        let token = auth::new_token();
        if !db::token_exists(conn, &token)? {
            return Ok(token);
        }
    }
}

pub fn register(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::RegisterRequest,
) -> Result<proto::RegisterResponse> {
    with_ctx(conn, caller, |ctx| {
        if ctx.caller.token.is_some() {
            bail!("already registered; `bbs bye` first to register again");
        }
        let owner = ctx.human()?;
        if let Some(name) = &req.name {
            validate_principal_name(name, "agent")?;
        }
        let cutoff = fmt_time(ctx.now - chrono::Duration::days(RETIRE_AFTER_DAYS));
        for id in db::stale_agent_ids(ctx.conn, &cutoff)? {
            db::retire_agent(ctx.conn, id, &ctx.now_s)?;
            tracing::info!(agent_id = id, "retired stale agent");
        }
        let name = match req.name {
            Some(name) => {
                if db::taken_names(ctx.conn)?.contains(&name.to_ascii_lowercase()) {
                    bail!("agent name {name:?} is already reserved");
                }
                name
            }
            None => pick_name(ctx.conn)?,
        };
        let token = new_token(ctx.conn)?;
        let id = db::insert_agent(
            ctx.conn,
            &db::NewAgent {
                name: &name,
                token: &token,
                owner_id: owner.id,
                status: req.note.as_deref(),
                pwd: &req.pwd,
                hostname: &req.hostname,
                now: &ctx.now_s,
            },
        )?;
        tracing::info!(
            agent_id = id,
            name,
            pwd = req.pwd,
            hostname = req.hostname,
            "registered agent"
        );
        Ok(proto::RegisterResponse { name, token })
    })
}

pub fn whoami(conn: &mut Connection, caller: &Caller) -> Result<proto::Principal> {
    with_ctx(conn, caller, |ctx| Ok(principal(ctx.actor()?)))
}

pub fn set_status(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SetStatusRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        db::set_status(ctx.conn, me.id, &req.text, &ctx.now_s)?;
        Ok(proto::Empty {})
    })
}

pub fn bye(conn: &mut Connection, caller: &Caller) -> Result<proto::ByeResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        retire(ctx, &me, &me, &format!("Agent {:?} retired.", me.name))?;
        Ok(proto::ByeResponse { name: me.name })
    })
}

pub fn retire_agent(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::RetireRequest,
) -> Result<proto::ByeResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let Some(target) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
            bail!("no live agent named {:?}", req.agent);
        };
        let notice = format!("Human {:?} retired agent {:?}.", me.name, target.name);
        retire(ctx, &me, &target, &notice)?;
        Ok(proto::ByeResponse { name: target.name })
    })
}

/// Broadcast the retirement sentence and name the affected agent's membership
/// change in each group. Announce first: retirement drops the memberships.
fn retire(ctx: &Ctx, by: &db::Principal, target: &db::Principal, notice: &str) -> Result<()> {
    for (gid, group) in db::member_groups(ctx.conn, target.id)? {
        broadcast(
            ctx,
            by,
            gid,
            &format!(
                "{notice} Agent {:?} is no longer a member of group @{group}.",
                target.name
            ),
        )?;
    }
    db::retire_agent(ctx.conn, target.id, &ctx.now_s)?;
    tracing::info!(agent = target.name, by = by.name, "agent retired");
    Ok(())
}

// ---- groups ----------------------------------------------------------------

fn group_name(group: &str) -> &str {
    group.strip_prefix('@').unwrap_or(group)
}

fn require_group(conn: &Connection, group: &str) -> Result<i64> {
    db::group_id(conn, group)?.with_context(|| format!("no such group @{group}"))
}

pub fn group_register(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupRegisterRequest,
) -> Result<proto::GroupResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        validate_group_name(group)?;
        let description = req
            .description
            .as_deref()
            .map(validate_group_description)
            .transpose()?;
        if db::group_id(ctx.conn, group)?.is_some() {
            bail!("group @{group} already exists");
        }
        db::insert_group(ctx.conn, group, description, &ctx.now_s)?;
        tracing::info!(group, by = me.name, "registered group");
        Ok(proto::GroupResponse {
            group: group.to_string(),
        })
    })
}

pub fn group_describe(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupDescribeRequest,
) -> Result<proto::GroupResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        let description = req
            .description
            .as_deref()
            .map(validate_group_description)
            .transpose()?;
        let gid = require_group(ctx.conn, group)?;
        db::set_group_description(ctx.conn, gid, description)?;
        tracing::info!(
            group,
            by = me.name,
            cleared = description.is_none(),
            "described group"
        );
        Ok(proto::GroupResponse {
            group: group.to_string(),
        })
    })
}

pub fn group_delete(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupRequest,
) -> Result<proto::GroupResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        let gid = require_group(ctx.conn, group)?;
        // Notify current members before archiving removes the group's access scope.
        broadcast(
            ctx,
            &me,
            gid,
            &format!("Human {:?} archived group @{group}.", me.name),
        )?;
        db::archive_group(ctx.conn, gid, &ctx.now_s)?;
        Ok(proto::GroupResponse {
            group: group.to_string(),
        })
    })
}

pub fn group_join(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupRequest,
) -> Result<proto::GroupJoinResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        let group = group_name(&req.group);
        let gid = require_group(ctx.conn, group)?;
        let joined = db::join_group(ctx.conn, gid, me.id, &ctx.now_s)?;
        if joined {
            broadcast(
                ctx,
                &me,
                gid,
                &format!("Agent {:?} joined group @{group}.", me.name),
            )?;
            tracing::info!(agent = me.name, group, "joined group");
        }
        Ok(proto::GroupJoinResponse {
            group: group.to_string(),
            joined,
        })
    })
}

pub fn group_leave(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupRequest,
) -> Result<proto::GroupResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        let group = group_name(&req.group);
        let gid = require_group(ctx.conn, group)?;
        if !db::is_member(ctx.conn, gid, me.id)? {
            bail!("you are not in @{group}");
        }
        broadcast(
            ctx,
            &me,
            gid,
            &format!("Agent {:?} left group @{group}.", me.name),
        )?;
        db::leave_group(ctx.conn, gid, me.id)?;
        tracing::info!(agent = me.name, group, "left group");
        Ok(proto::GroupResponse {
            group: group.to_string(),
        })
    })
}

pub fn group_remove(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupRemoveRequest,
) -> Result<proto::GroupRemoveResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        let gid = require_group(ctx.conn, group)?;
        let Some(target) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
            bail!("no live agent named {:?}", req.agent);
        };
        if !db::is_member(ctx.conn, gid, target.id)? {
            bail!("{} is not in @{group}", target.name);
        }
        // Notify before removing so the removed agent learns why the group went quiet.
        let notice = format!(
            "Human {:?} removed agent {:?} from group @{group}.",
            me.name, target.name
        );
        broadcast(ctx, &me, gid, &notice)?;
        db::leave_group(ctx.conn, gid, target.id)?;
        tracing::info!(
            agent = target.name,
            group,
            by = me.name,
            "removed from group"
        );
        Ok(proto::GroupRemoveResponse {
            group: group.to_string(),
            agent: target.name,
        })
    })
}

pub fn group_list(conn: &mut Connection, caller: &Caller) -> Result<proto::GroupListResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let only = (me.kind == db::Kind::Agent).then_some(me.id);
        let groups = db::list_groups(ctx.conn, only)?
            .into_iter()
            .map(|g| proto::Group {
                name: g.name,
                members: g.members,
                description: g.description,
            })
            .collect();
        Ok(proto::GroupListResponse { groups })
    })
}

// ---- messages --------------------------------------------------------------

/// Deliver `body` from `me` to every member of the group and every human,
/// except `me`. Returns (recipients, recipients visible to `me`): humans
/// silently receive every broadcast, so agents only see the agent count.
fn broadcast(ctx: &Ctx, me: &db::Principal, gid: i64, body: &str) -> Result<(usize, usize)> {
    let msg = db::insert_message(ctx.conn, me, Some(gid), body, &ctx.now_s)?;
    let mut count = 0;
    let mut visible = 0;
    let agents = db::group_member_ids(ctx.conn, gid)?
        .into_iter()
        .map(|id| (db::Kind::Agent, id));
    let humans = db::human_ids(ctx.conn)?
        .into_iter()
        .map(|id| (db::Kind::Human, id));
    for (kind, id) in agents.chain(humans) {
        if kind == me.kind && id == me.id {
            continue;
        }
        db::insert_delivery(ctx.conn, msg, kind, id)?;
        count += 1;
        if me.kind == db::Kind::Human || kind == db::Kind::Agent {
            visible += 1;
        }
    }
    Ok((count, visible))
}

pub fn send(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SendRequest,
) -> Result<proto::SendResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let (to, body) = (req.to.as_str(), req.body.as_str());
        validate_body(body)?;

        if let Some(group) = to.strip_prefix('@') {
            let gid = require_group(ctx.conn, group)?;
            if me.kind == db::Kind::Agent && !db::is_member(ctx.conn, gid, me.id)? {
                bail!("you are not a member of @{group}");
            }
            let (count, visible) = broadcast(ctx, &me, gid, body)?;
            tracing::info!(
                from = me.name,
                to,
                recipients = count,
                bytes = body.len(),
                "sent broadcast"
            );
            return Ok(proto::SendResponse {
                to: format!("@{group}"),
                recipients: Some(visible as u32),
            });
        }

        let target = match db::find_human(ctx.conn, to)? {
            Some(h) => h,
            None => match db::live_agent_by_name(ctx.conn, to)? {
                Some(a) => a,
                None if db::retired_agent_exists(ctx.conn, to)? => {
                    bail!("{to} is offline (retired)")
                }
                None => bail!("no such agent or human: {to}"),
            },
        };
        if target.kind == me.kind && target.id == me.id {
            bail!("cannot send a message to yourself");
        }
        if me.kind == db::Kind::Agent && target.kind == db::Kind::Human {
            bail!(
                "agents cannot message humans; talk to {} through your harness",
                target.name
            );
        }
        if me.kind == db::Kind::Agent
            && target.kind == db::Kind::Agent
            && !db::share_group(ctx.conn, me.id, target.id)?
        {
            bail!("{} is not in any of your groups", target.name);
        }
        let msg = db::insert_message(ctx.conn, &me, None, body, &ctx.now_s)?;
        db::insert_delivery(ctx.conn, msg, target.kind, target.id)?;
        tracing::info!(
            from = me.name,
            to = target.name,
            bytes = body.len(),
            "sent message"
        );
        Ok(proto::SendResponse {
            to: target.name,
            recipients: None,
        })
    })
}

/// Unread mail oldest first, archived as returned; or archived history.
pub fn read(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReadRequest,
) -> Result<proto::ReadResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let msgs = if req.archived {
            let since = req
                .since
                .as_deref()
                .map(|s| parse_time("since", s))
                .transpose()?;
            db::archived_since(ctx.conn, &me, since.as_deref(), req.limit)?
        } else {
            let from_kind = match req.from_kind.map(proto::Kind::try_from) {
                Some(Ok(proto::Kind::Agent)) => Some(db::Kind::Agent),
                Some(Ok(proto::Kind::Human)) => Some(db::Kind::Human),
                _ => None,
            };
            let msgs = db::take_unread(ctx.conn, &me, from_kind, req.limit, &ctx.now_s)?;
            if !msgs.is_empty() {
                tracing::info!(reader = me.name, count = msgs.len(), "archived messages");
            }
            msgs
        };
        let messages = msgs
            .into_iter()
            .map(|m| message(m, Some(&me.name)))
            .collect();
        Ok(proto::ReadResponse { messages })
    })
}

pub fn list_agents(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ListAgentsRequest,
) -> Result<proto::ListAgentsResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let mut groups = db::agent_groups(ctx.conn)?;
        let mut rows = db::list_agents(ctx.conn, req.all && me.kind == db::Kind::Human)?;
        if me.kind == db::Kind::Agent {
            let mine: HashSet<&String> = groups.get(&me.id).into_iter().flatten().collect();
            rows.retain(|a| {
                a.id == me.id
                    || groups
                        .get(&a.id)
                        .into_iter()
                        .flatten()
                        .any(|g| mine.contains(g))
            });
        }
        // Agents cannot message humans, so humans are not listed for them.
        let humans = match me.kind {
            db::Kind::Human => db::list_humans(ctx.conn)?,
            db::Kind::Agent => Vec::new(),
        };
        let humans = humans
            .into_iter()
            .map(|h| proto::Human {
                name: h.name,
                active_at: h.active_at,
            })
            .collect();
        let agents = rows
            .into_iter()
            .map(|a| proto::Agent {
                groups: groups.remove(&a.id).unwrap_or_default(),
                name: a.name,
                status: a.status,
                pwd: a.pwd,
                hostname: a.hostname,
                active_at: a.active_at,
                retired: a.retired_at.is_some(),
                owner: a.owner,
            })
            .collect();
        Ok(proto::ListAgentsResponse { humans, agents })
    })
}

/// Normalize a client-supplied RFC3339 time to the stored fixed-width form.
fn parse_time(field: &str, s: &str) -> Result<String> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| fmt_time(t.with_timezone(&Utc)))
        .with_context(|| format!("invalid {field} {s:?}: expected RFC3339"))
}

/// Largest feed window or batch a client may request.
pub const MAX_FEED_WINDOW: u32 = 1000;

/// The most recent `backlog` messages matching the request's filters, oldest
/// first; with `after_id`, the oldest `backlog` matching messages after it.
/// Humans only; never archives anything. Unknown names fail loudly rather than
/// matching nothing, and so does a window outside 1..=MAX_FEED_WINDOW: clamping
/// would silently return something other than what was asked for.
pub fn feed(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::FeedRequest,
) -> Result<proto::FeedSnapshot> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        if !(1..=MAX_FEED_WINDOW).contains(&req.backlog) {
            bail!(
                "feed window must be 1 to {MAX_FEED_WINDOW} messages, not {}",
                req.backlog
            );
        }
        let group_ids = req
            .group
            .as_deref()
            .map(group_name)
            .map(|group| {
                let ids = db::group_ids_named(ctx.conn, group)?;
                if ids.is_empty() {
                    bail!("no such group @{group}");
                }
                Ok(ids)
            })
            .transpose()?;
        let senders = req
            .from
            .as_deref()
            .map(|from| {
                let ids = db::principals_named(ctx.conn, from)?;
                if ids.is_empty() {
                    bail!("no agent or human named {from:?}");
                }
                Ok(ids)
            })
            .transpose()?;
        let from_human = req
            .from_human
            .as_deref()
            .map(|name| {
                db::find_human(ctx.conn, name)?
                    .map(|h| h.id)
                    .with_context(|| format!("unknown human {name:?}"))
            })
            .transpose()?;
        let since = req
            .since
            .as_deref()
            .map(|s| parse_time("since", s))
            .transpose()?;
        let until = req
            .until
            .as_deref()
            .map(|s| parse_time("until", s))
            .transpose()?;
        if let (Some(since), Some(until)) = (&since, &until)
            && since >= until
        {
            bail!("since ({since}) must be earlier than until ({until})");
        }
        let filter = db::FeedFilter {
            group_ids,
            senders,
            from_human,
            since: since.as_deref(),
            until: until.as_deref(),
            scope: req.mine.then_some(me.id),
            after_id: req.after_id,
        };
        let messages = db::feed_messages(ctx.conn, req.backlog, &filter)?
            .into_iter()
            .map(|m| message(m, None))
            .collect();
        Ok(proto::FeedSnapshot { messages })
    })
}

/// After sending a cursor-mode snapshot, move the cursor past it. Returns
/// whether the batch was full, so more matching messages may be waiting and
/// the feed should fetch again before waiting for the next change.
pub fn advance_feed_cursor(req: &mut proto::FeedRequest, sent: &proto::FeedSnapshot) -> bool {
    let Some(after) = &mut req.after_id else {
        return false;
    };
    if let Some(last) = sent.messages.last() {
        *after = last.id;
    }
    sent.messages.len() >= req.backlog as usize
}

// ---- harness sessions ------------------------------------------------------

pub fn admin_issue_token(conn: &mut Connection, name: &str, create: bool) -> Result<String> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        if create {
            insert_human(ctx, name)?;
        }
        let human =
            db::find_human(ctx.conn, name)?.with_context(|| format!("unknown human {name:?}"))?;
        let token = auth::new_token();
        db::set_human_token(ctx.conn, human.id, &auth::token_hash(&token))?;
        Ok(token)
    })
}

pub fn admin_revoke_token(conn: &mut Connection, name: &str) -> Result<()> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let human =
            db::find_human(ctx.conn, name)?.with_context(|| format!("unknown human {name:?}"))?;
        db::revoke_human_token(ctx.conn, human.id)
    })
}

/// Bind a harness session to the agent owning `token`. A session holds one
/// identity, so an agent it was previously bound to is retired. An unknown
/// token binds nothing and returns no agent.
pub fn bind_session(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::BindSessionRequest,
) -> Result<proto::BindSessionResponse> {
    with_ctx(conn, caller, |ctx| {
        let human = caller
            .require_human_token
            .then(|| ctx.human())
            .transpose()?;
        let Some(agent) = db::agent_by_token(ctx.conn, &req.token)? else {
            return Ok(proto::BindSessionResponse { agent: None });
        };
        if let Some(human) = human {
            if db::agent_owner_id(ctx.conn, agent.id)? != Some(human.id)
                || !auth::is_strong_token(&req.token)
            {
                bail!("cannot bind an agent owned by another human or using a legacy token");
            }
            if let Some(previous) = db::session_binding(ctx.conn, &req.harness, &req.session_id)?
                && db::agent_owner_id(ctx.conn, previous)? != Some(human.id)
            {
                bail!("session belongs to another human");
            }
        }
        let previous = db::bind_session(
            ctx.conn,
            &req.harness,
            &req.session_id,
            agent.id,
            &ctx.now_s,
        )?;
        if let Some(previous) = previous {
            db::retire_agent(ctx.conn, previous, &ctx.now_s)?;
            tracing::info!(agent_id = previous, "retired agent replaced in session");
        }
        tracing::info!(
            harness = req.harness,
            session_id = req.session_id,
            agent = agent.name,
            "bound session"
        );
        Ok(proto::BindSessionResponse {
            agent: Some(principal(agent)),
        })
    })
}

/// The live agent bound to a harness session, with its token; both unset if unbound.
pub fn session_agent(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SessionAgentRequest,
) -> Result<proto::SessionAgentResponse> {
    with_ctx(conn, caller, |ctx| {
        let human = caller
            .require_human_token
            .then(|| ctx.human())
            .transpose()?;
        let Some((agent, token)) = db::session_agent(ctx.conn, &req.harness, &req.session_id)?
        else {
            return Ok(proto::SessionAgentResponse::default());
        };
        if let Some(human) = human
            && (db::agent_owner_id(ctx.conn, agent.id)? != Some(human.id)
                || !auth::is_strong_token(&token))
        {
            return Ok(proto::SessionAgentResponse::default());
        }
        Ok(proto::SessionAgentResponse {
            agent: Some(principal(agent)),
            token,
        })
    })
}
