//! Business logic: one function per command over a SQLite connection, taking
//! and returning the protobuf types. Each runs in its own `IMMEDIATE`
//! transaction, generates `now`, and resolves the caller. It knows nothing
//! about sockets, files or output formatting.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use rand::seq::IndexedRandom;
use rusqlite::{Connection, TransactionBehavior};

use crate::auth::{self, AuthenticationError};
use crate::db;
use crate::names::NAMES;
use crate::proto;
use crate::timer::Schedule;

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
        to_owner: m.to_owner,
        read_at: m.read_at,
        body: m.body,
        control: match m.control.as_deref() {
            Some("cancel") => Some(proto::Control::Cancel.into()),
            Some("set-effort") => Some(proto::Control::SetEffort.into()),
            _ => None,
        },
        timer_id: m.timer_id,
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

/// Replace the token of an agent the calling human owns.
pub fn rotate_agent_token(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::RetireRequest,
) -> Result<proto::RegisterResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let Some(target) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
            bail!("no live agent named {:?}", req.agent);
        };
        if db::agent_owner_id(ctx.conn, target.id)? != Some(me.id) {
            bail!("{} is not your agent", target.name);
        }
        let token = new_token(ctx.conn)?;
        db::set_agent_token(ctx.conn, target.id, &token)?;
        tracing::info!(agent = target.name, by = me.name, "rotated agent token");
        Ok(proto::RegisterResponse {
            name: target.name,
            token,
        })
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
        let orphaned = departing_writer(ctx, gid, &group, target)?;
        broadcast(
            ctx,
            by,
            gid,
            &format!(
                "{notice} Agent {:?} is no longer a member of group @{group}.{orphaned}",
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
                &format!(
                    "Agent {:?} joined group @{group}{}.",
                    me.name,
                    if db::is_writer(ctx.conn, gid, me.id)? {
                        " as a writer"
                    } else {
                        ""
                    }
                ),
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
        let orphaned = departing_writer(ctx, gid, group, &me)?;
        broadcast(
            ctx,
            &me,
            gid,
            &format!("Agent {:?} left group @{group}.{orphaned}", me.name),
        )?;
        db::leave_group(ctx.conn, gid, me.id)?;
        tracing::info!(agent = me.name, group, "left group");
        Ok(proto::GroupResponse {
            group: group.to_string(),
        })
    })
}

pub fn group_add(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::GroupAddRequest,
) -> Result<proto::GroupAddResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        if req.agents.is_empty() {
            bail!("name at least one agent to add");
        }
        let gid = require_group(ctx.conn, group)?;
        // Check every agent before adding any, so a bad name changes nothing.
        let mut targets: Vec<db::Principal> = Vec::new();
        for name in &req.agents {
            let Some(target) = db::live_agent_by_name(ctx.conn, name)? else {
                bail!("no live agent named {name:?}");
            };
            if !targets.iter().any(|t| t.id == target.id) {
                targets.push(target);
            }
        }
        let (mut added, mut already) = (Vec::new(), Vec::new());
        for target in targets {
            if !db::join_group(ctx.conn, gid, target.id, &ctx.now_s)? {
                already.push(target.name);
                continue;
            }
            let writer = if db::is_writer(ctx.conn, gid, target.id)? {
                " as a writer"
            } else {
                ""
            };
            let notice = format!(
                "Human {:?} added agent {:?} to group @{group}{writer}.",
                me.name, target.name
            );
            broadcast(ctx, &me, gid, &notice)?;
            tracing::info!(agent = target.name, group, by = me.name, "added to group");
            added.push(target.name);
        }
        Ok(proto::GroupAddResponse {
            group: group.to_string(),
            added,
            already,
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
        let orphaned = departing_writer(ctx, gid, group, &target)?;
        let notice = format!(
            "Human {:?} removed agent {:?} from group @{group}.{orphaned}",
            me.name, target.name
        );
        broadcast(ctx, &me, gid, &notice)?;
        db::leave_group(ctx.conn, gid, target.id)?;
        // Agents join groups themselves, so removal also takes write away.
        db::set_writer(ctx.conn, gid, target.id, false, &ctx.now_s)?;
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

pub fn set_group_writer(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SetGroupWriterRequest,
) -> Result<proto::SetGroupWriterResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let group = group_name(&req.group);
        let gid = require_group(ctx.conn, group)?;
        let Some(target) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
            bail!("no live agent named {:?}", req.agent);
        };
        // Granting needs a member; revoking does not, so a grant can be
        // removed after the agent leaves.
        if req.grant && !db::is_member(ctx.conn, gid, target.id)? {
            bail!("{} is not in @{group}", target.name);
        }
        let had_writers = !db::group_writers(ctx.conn, gid)?.is_empty();
        let changed = db::set_writer(ctx.conn, gid, target.id, req.grant, &ctx.now_s)?;
        let agent = target.name;
        if changed {
            let notice = if req.grant {
                format!(
                    "Human {:?} granted write on @{group} to agent {agent:?}.",
                    me.name
                )
            } else {
                let orphaned = had_writers && db::group_writers(ctx.conn, gid)?.is_empty();
                format!(
                    "Human {:?} revoked write on @{group} from agent {agent:?}.{}",
                    me.name,
                    if orphaned {
                        no_writers(group)
                    } else {
                        String::new()
                    }
                )
            };
            broadcast(ctx, &me, gid, &notice)?;
            tracing::info!(
                agent,
                group,
                grant = req.grant,
                by = me.name,
                "changed group writer"
            );
        }
        Ok(proto::SetGroupWriterResponse {
            group: group.to_string(),
            agent,
            changed,
        })
    })
}

fn no_writers(group: &str) -> String {
    format!(" @{group} now has no agent writers.")
}

/// The no-writers sentence when `agent`, about to leave, is the group's only
/// writer among its members; else empty.
fn departing_writer(ctx: &Ctx, gid: i64, group: &str, agent: &db::Principal) -> Result<String> {
    let only =
        db::group_writers(ctx.conn, gid)?.len() == 1 && db::is_writer(ctx.conn, gid, agent.id)?;
    Ok(if only {
        no_writers(group)
    } else {
        String::new()
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
                writers: Some(proto::GroupWriters { names: g.writers }),
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

/// A control message: from a human, direct to a live agent that human owns.
fn send_control(
    ctx: &Ctx,
    me: &db::Principal,
    to: &str,
    body: &str,
    control: i32,
) -> Result<proto::SendResponse> {
    let control = match proto::Control::try_from(control) {
        Ok(proto::Control::Cancel) => "cancel",
        Ok(proto::Control::SetEffort) => {
            // The body is the requested level; reports match it exactly.
            if body.is_empty() || body.len() > MAX_EFFORT_VALUE {
                bail!("an effort request names a level of at most {MAX_EFFORT_VALUE} bytes");
            }
            "set-effort"
        }
        _ => bail!("unknown control {control}"),
    };
    if me.kind != db::Kind::Human {
        bail!("only humans can send control messages");
    }
    if to.starts_with('@') {
        bail!("control messages go to one agent, not a group");
    }
    let Some(target) = db::live_agent_by_name(ctx.conn, to)? else {
        bail!("no live agent named {to:?}");
    };
    if db::agent_owner_id(ctx.conn, target.id)? != Some(me.id) {
        bail!("{} is not your agent", target.name);
    }
    let msg = db::insert_message(ctx.conn, me, None, body, &ctx.now_s)?;
    db::set_message_control(ctx.conn, msg, control)?;
    db::insert_delivery(ctx.conn, msg, db::Kind::Agent, target.id)?;
    tracing::info!(
        from = me.name,
        to = target.name,
        control,
        "sent control message"
    );
    Ok(proto::SendResponse {
        to: target.name,
        recipients: None,
        id: Some(msg),
    })
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
        if let Some(control) = req.control {
            return send_control(ctx, &me, to, body, control);
        }

        if let Some(group) = to.strip_prefix('@') {
            let gid = require_group(ctx.conn, group)?;
            if me.kind == db::Kind::Agent {
                if !db::is_member(ctx.conn, gid, me.id)? {
                    bail!("you are not a member of @{group}");
                }
                if !db::is_writer(ctx.conn, gid, me.id)? {
                    let writers = db::group_writers(ctx.conn, gid)?;
                    let denied =
                        format!("you lack write access to @{group}; only a human can grant it");
                    if writers.is_empty() {
                        bail!("{denied}. @{group} has no agent writers.");
                    }
                    bail!(
                        "{denied}. Writers: {} (message one of them to reach the group).",
                        writers.join(", ")
                    );
                }
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
                id: None,
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
        if me.kind == db::Kind::Agent
            && target.kind == db::Kind::Agent
            && !db::share_group(ctx.conn, me.id, target.id)?
        {
            bail!("{} is not in any of your groups", target.name);
        }
        let msg = db::insert_message(ctx.conn, &me, None, body, &ctx.now_s)?;
        db::insert_delivery(ctx.conn, msg, target.kind, target.id)?;
        match (me.kind, target.kind) {
            (db::Kind::Human, db::Kind::Agent) => {
                db::record_human_conversation(ctx.conn, target.id, me.id, &ctx.now_s)?;
            }
            (db::Kind::Agent, db::Kind::Human) => {
                db::record_human_conversation(ctx.conn, me.id, target.id, &ctx.now_s)?;
            }
            _ => {}
        }
        tracing::info!(
            from = me.name,
            to = target.name,
            bytes = body.len(),
            "sent message"
        );
        Ok(proto::SendResponse {
            to: target.name,
            recipients: None,
            id: Some(msg),
        })
    })
}

/// Unread mail oldest first, archived as returned; or archived history.
/// Mark read the caller's unread mail from `from`, as viewing it in
/// `bbs connect` does.
pub fn mark_read(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::MarkReadRequest,
) -> Result<proto::MarkReadResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let from = match db::find_human(ctx.conn, &req.from)? {
            Some(h) => h,
            None => db::live_agent_by_name(ctx.conn, &req.from)?
                .with_context(|| format!("no such agent or human: {}", req.from))?,
        };
        let count = db::mark_read_from(ctx.conn, &me, &from, &ctx.now_s)?;
        Ok(proto::MarkReadResponse {
            count: count as u32,
        })
    })
}

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
            let msgs = db::take_unread(ctx.conn, &me, req.limit, &ctx.now_s)?;
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
        // Anyone may message any human, so every caller sees them all.
        let humans = db::list_humans(ctx.conn)?
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
                last_sent_at: a.last_sent_at,
                effort: a.effort.as_deref().and_then(effort_from_json),
                effort_reported_at: a.effort_reported_at,
                last_human_conversation_at: a.last_human_conversation_at,
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
        let with = req
            .with
            .as_deref()
            .map(|name| {
                let ids = db::principals_named(ctx.conn, name)?;
                if ids.is_empty() {
                    bail!("no agent or human named {name:?}");
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
            with,
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

// ---- agent effort (agent-effort.v1) -----------------------------------------

/// Bounds on one effort report.
const MAX_EFFORT_CHOICES: usize = 32;
const MAX_EFFORT_VALUE: usize = 128;
const MAX_EFFORT_TEXT: usize = 512;

/// Store the calling agent's own effort report after validation.
pub fn report_effort(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReportEffortRequest,
) -> Result<proto::ReportEffortResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        let report = req.report.as_ref().context("missing report")?;
        validate_effort(ctx, me.id, report)?;
        let json = effort_json(report);
        db::set_effort(ctx.conn, me.id, &json, &ctx.now_s)?;
        Ok(proto::ReportEffortResponse {})
    })
}

fn validate_effort(ctx: &Ctx, agent: i64, report: &proto::EffortReport) -> Result<()> {
    let state = report.state.as_ref().context("report without state")?;
    if state.choices.len() > MAX_EFFORT_CHOICES {
        bail!("{} choices; the limit is {MAX_EFFORT_CHOICES}", state.choices.len());
    }
    let mut seen = HashSet::new();
    for choice in &state.choices {
        if choice.value.is_empty()
            || choice.value.len() > MAX_EFFORT_VALUE
            || choice.label.len() > MAX_EFFORT_VALUE
        {
            bail!("invalid choice {choice:?}");
        }
        if !seen.insert(&choice.value) {
            bail!("duplicate effort choice {:?}", choice.value);
        }
    }
    if state.reason.len() > MAX_EFFORT_TEXT {
        bail!("reason is too long");
    }
    if let Some(current) = &state.current
        && current.len() > MAX_EFFORT_VALUE
    {
        bail!("current is too long");
    }
    if let Some(pending) = &report.pending {
        check_effort_request(ctx, agent, pending.request_id)?;
        if pending.value.is_empty() {
            bail!("pending request without a value");
        }
        if pending.value.len() > MAX_EFFORT_VALUE {
            bail!("requested value is too long");
        }
        let body = db::effort_control_body(ctx.conn, agent, pending.request_id)?
            .expect("checked above");
        if pending.value != body {
            bail!(
                "pending request {} value {:?} does not match the control's {:?}",
                pending.request_id,
                pending.value,
                body
            );
        }
    }
    if let Some(outcome) = &report.answered {
        if outcome.result() == proto::effort_outcome::Result::Unspecified {
            bail!("unspecified outcome");
        }
        if outcome.detail.len() > MAX_EFFORT_TEXT {
            bail!("outcome detail is too long");
        }
        check_effort_request(ctx, agent, outcome.request_id)?;
    }
    if let (Some(pending), Some(answered)) = (&report.pending, &report.answered)
        && pending.request_id == answered.request_id
    {
        bail!("request {} is both pending and answered", pending.request_id);
    }
    Ok(())
}

/// Report references must name a set-effort control delivered to this agent.
fn check_effort_request(ctx: &Ctx, agent: i64, id: i64) -> Result<()> {
    if id <= 0 {
        bail!("invalid request id {id}");
    }
    if db::effort_control_body(ctx.conn, agent, id)?.is_none() {
        bail!("request id {id} is not a set-effort control delivered to this agent");
    }
    Ok(())
}

/// A stored report back to its typed form; validated at write, so a parse
/// failure here means hand-edited storage and reads as no report.
fn effort_from_json(stored: &str) -> Option<proto::EffortReport> {
    let v: serde_json::Value = serde_json::from_str(stored).ok()?;
    let state = v.get("state")?;
    let choices = state
        .get("choices")?
        .as_array()?
        .iter()
        .map(|c| {
            Some(proto::EffortChoice {
                value: c.get("value")?.as_str()?.to_string(),
                label: c.get("label")?.as_str()?.to_string(),
                rank: c.get("rank").and_then(|r| r.as_u64()).map(|r| r as u32),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let request = |v: &serde_json::Value| {
        Some(proto::EffortRequest {
            request_id: v.get("request_id")?.as_i64()?,
            value: v.get("value")?.as_str()?.to_string(),
        })
    };
    let outcome = v.get("answered").and_then(|o| {
        if o.is_null() {
            return None;
        }
        Some(proto::EffortOutcome {
            request_id: o.get("request_id")?.as_i64()?,
            result: proto::effort_outcome::Result::from_str_name(
                o.get("result")?.as_str()?,
            )? as i32,
            detail: o.get("detail")?.as_str()?.to_string(),
        })
    });
    Some(proto::EffortReport {
        state: Some(proto::EffortState {
            supported: state.get("supported")?.as_bool()?,
            reason: state.get("reason")?.as_str()?.to_string(),
            choices,
            current: state
                .get("current")
                .and_then(|c| c.as_str().map(str::to_string)),
        }),
        pending: v.get("pending").and_then(|p| {
            if p.is_null() {
                None
            } else {
                request(p)
            }
        }),
        answered: outcome,
    })
}

/// The validated report as stored JSON.
fn effort_json(report: &proto::EffortReport) -> String {
    let state = report.state.as_ref().expect("validated");
    serde_json::json!({
        "state": {
            "supported": state.supported,
            "reason": state.reason,
            "choices": state.choices.iter().map(|c| serde_json::json!({
                "value": c.value,
                "label": c.label,
                "rank": c.rank,
            })).collect::<Vec<_>>(),
            "current": state.current,
        },
        "pending": report.pending.as_ref().map(|p| serde_json::json!({
            "request_id": p.request_id,
            "value": p.value,
        })),
        "answered": report.answered.as_ref().map(|o| serde_json::json!({
            "request_id": o.request_id,
            "result": o.result().as_str_name(),
            "detail": o.detail,
        })),
    })
    .to_string()
}

// ---- agent activity --------------------------------------------------------

/// The largest activity record accepted; senders cut long text to fit.
pub const MAX_ACTIVITY_BYTES: usize = 8 * 1024;

/// Store a batch of the calling agent's activity in one transaction, trimming
/// its history. Each record must be a JSON object with a string "event".
pub fn post_activity(
    conn: &mut Connection,
    caller: &Caller,
    events: Vec<proto::ActivityEvent>,
) -> Result<u32> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        let rows = events
            .into_iter()
            .map(|e| Ok((activity_event(&e.record)?, e.record)))
            .collect::<Result<Vec<_>>>()?;
        db::post_activity(ctx.conn, me.id, &ctx.now_s, &rows)?;
        Ok(rows.len() as u32)
    })
}

/// The "event" of an activity record, checking its size and shape.
fn activity_event(record: &str) -> Result<String> {
    if record.len() > MAX_ACTIVITY_BYTES {
        bail!(
            "activity record is {} bytes; the limit is {MAX_ACTIVITY_BYTES}",
            record.len()
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(record).context("activity record is not JSON")?;
    value
        .get("event")
        .and_then(|e| e.as_str())
        .map(str::to_string)
        .context("activity record has no string \"event\"")
}

/// The newest `backlog` events from every agent, oldest first; with
/// `after_id`, the oldest `backlog` after it. Humans only.
pub fn activity(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ActivityRequest,
) -> Result<proto::ActivitySnapshot> {
    with_ctx(conn, caller, |ctx| {
        ctx.human()?;
        if !(1..=MAX_FEED_WINDOW).contains(&req.backlog) {
            bail!(
                "activity window must be 1 to {MAX_FEED_WINDOW} events, not {}",
                req.backlog
            );
        }
        let agents = req
            .agent
            .as_deref()
            .map(|name| {
                let ids: Vec<i64> = db::principals_named(ctx.conn, name)?
                    .into_iter()
                    .filter(|(kind, _)| *kind == db::Kind::Agent)
                    .map(|(_, id)| id)
                    .collect();
                if ids.is_empty() {
                    bail!("no agent named {name:?}");
                }
                Ok(ids)
            })
            .transpose()?;
        let events = db::activity_page(ctx.conn, req.backlog, req.after_id, agents.as_deref())?
            .into_iter()
            .map(|r| proto::Activity {
                id: r.id,
                created_at: r.created_at,
                agent: r.agent,
                owner: r.owner,
                record: r.record,
            })
            .collect();
        Ok(proto::ActivitySnapshot { events })
    })
}

/// As `advance_feed_cursor`, for activity.
pub fn advance_activity_cursor(
    req: &mut proto::ActivityRequest,
    sent: &proto::ActivitySnapshot,
) -> bool {
    let Some(after) = &mut req.after_id else {
        return false;
    };
    if let Some(last) = sent.events.last() {
        *after = last.id;
    }
    sent.events.len() >= req.backlog as usize
}

/// A query a watch stream re-runs: a window of the newest rows, or a cursor
/// that moves past each batch it sends.
pub trait Watched: Clone + Send + Sync + 'static {
    type Snapshot: Clone + PartialEq + Send + 'static;
    /// Send a snapshot only when it differs from the last one sent.
    const ONLY_CHANGES: bool = false;
    fn fetch(conn: &mut Connection, caller: &Caller, req: Self) -> Result<Self::Snapshot>;
    /// Move a cursor past `sent`; `true` when the batch was full, so more may
    /// be waiting.
    fn advance(&mut self, sent: &Self::Snapshot) -> bool;
    fn has_cursor(&self) -> bool;
    fn is_empty(snapshot: &Self::Snapshot) -> bool;
}

impl Watched for proto::FeedRequest {
    type Snapshot = proto::FeedSnapshot;
    fn fetch(conn: &mut Connection, caller: &Caller, req: Self) -> Result<Self::Snapshot> {
        feed(conn, caller, req)
    }
    fn advance(&mut self, sent: &Self::Snapshot) -> bool {
        advance_feed_cursor(self, sent)
    }
    fn has_cursor(&self) -> bool {
        self.after_id.is_some()
    }
    fn is_empty(snapshot: &Self::Snapshot) -> bool {
        snapshot.messages.is_empty()
    }
}

impl Watched for proto::ActivityRequest {
    type Snapshot = proto::ActivitySnapshot;
    fn fetch(conn: &mut Connection, caller: &Caller, req: Self) -> Result<Self::Snapshot> {
        activity(conn, caller, req)
    }
    fn advance(&mut self, sent: &Self::Snapshot) -> bool {
        advance_activity_cursor(self, sent)
    }
    fn has_cursor(&self) -> bool {
        self.after_id.is_some()
    }
    fn is_empty(snapshot: &Self::Snapshot) -> bool {
        snapshot.events.is_empty()
    }
}

// ---- what humans have yet to read or see (agent-summary.v1) -------------------

pub fn agent_summary(
    conn: &mut Connection,
    caller: &Caller,
    _req: proto::AgentSummaryRequest,
) -> Result<proto::AgentSummarySnapshot> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let agents = db::agent_summaries(ctx.conn, me.id)?
            .into_iter()
            .map(|s| proto::AgentSummary {
                agent: s.agent,
                unread_messages: s.unread_messages,
                oldest_unread_message: s.oldest_unread_message,
                unseen_stream: s.unseen_stream,
                oldest_unseen_stream: s.oldest_unseen_stream,
                last_sent_at: s.last_sent_at,
                effort: s.effort.as_deref().and_then(effort_from_json),
                effort_reported_at: s.effort_reported_at,
                last_human_conversation_at: s.last_human_conversation_at,
            })
            .collect();
        Ok(proto::AgentSummarySnapshot { agents })
    })
}

impl Watched for proto::AgentSummaryRequest {
    type Snapshot = proto::AgentSummarySnapshot;
    const ONLY_CHANGES: bool = true;
    fn fetch(conn: &mut Connection, caller: &Caller, req: Self) -> Result<Self::Snapshot> {
        agent_summary(conn, caller, req)
    }
    fn advance(&mut self, _sent: &Self::Snapshot) -> bool {
        false
    }
    fn has_cursor(&self) -> bool {
        false
    }
    fn is_empty(snapshot: &Self::Snapshot) -> bool {
        snapshot.agents.is_empty()
    }
}

pub fn mark_seen(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::MarkSeenRequest,
) -> Result<proto::MarkSeenResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let agent = db::live_agent_by_name(ctx.conn, &req.agent)?
            .with_context(|| format!("no such agent: {}", req.agent))?;
        let moved = db::mark_seen(ctx.conn, me.id, agent.id, req.through_id)?;
        Ok(proto::MarkSeenResponse { moved })
    })
}

// ---- timers ----------------------------------------------------------------

/// Server limits on timers, applied to each target agent.
#[derive(Clone, Copy, Debug)]
pub struct TimerLimits {
    pub max_per_agent: u32,
    /// The shortest time a recurring timer may wait between runs.
    pub min_interval: Duration,
}

impl Default for TimerLimits {
    fn default() -> TimerLimits {
        TimerLimits {
            max_per_agent: 20,
            min_interval: Duration::from_secs(5 * 60),
        }
    }
}

/// Whether an agent needs a shared group with another agent to set, list or
/// remove its timers, as `bbs send` does to message it. Waiting on the
/// human's decision; humans, and agents managing their own, never do.
const TIMERS_NEED_SHARED_GROUP: bool = true;

fn may_manage_timers(ctx: &Ctx, me: &db::Principal, target: &db::Principal) -> Result<()> {
    if TIMERS_NEED_SHARED_GROUP
        && me.kind == db::Kind::Agent
        && me.id != target.id
        && !db::share_group(ctx.conn, me.id, target.id)?
    {
        bail!("{} is not in any of your groups", target.name);
    }
    Ok(())
}

/// The live agent a timer is for.
fn timer_agent(ctx: &Ctx, name: &str) -> Result<db::Principal> {
    match db::live_agent_by_name(ctx.conn, name)? {
        Some(agent) => Ok(agent),
        None if db::retired_agent_exists(ctx.conn, name)? => bail!("{name} is retired"),
        None => bail!("no such agent: {name}"),
    }
}

fn timer_proto(t: db::TimerRow) -> proto::Timer {
    proto::Timer {
        id: t.id,
        agent: t.target,
        creator: t.creator,
        repeat: t.repeat,
        next_run: t.next_run,
        body: t.body,
    }
}

/// Set a timer that sends `body` to an agent when `when` comes.
pub fn timer_add(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::TimerAddRequest,
    limits: TimerLimits,
) -> Result<proto::Timer> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        validate_body(&req.body)?;
        let target = timer_agent(ctx, &req.agent)?;
        may_manage_timers(ctx, &me, &target)?;
        let schedule = Schedule::parse(&req.when, ctx.now)?;
        if let Some(gap) = schedule.shortest_gap(ctx.now)
            && gap < limits.min_interval
        {
            bail!(
                "{:?} runs as often as every {}; this server's shortest interval is {}",
                req.when.trim(),
                humantime::format_duration(gap),
                humantime::format_duration(limits.min_interval)
            );
        }
        if db::timer_count(ctx.conn, target.id)? >= i64::from(limits.max_per_agent) {
            bail!(
                "{} already has {} timers, this server's limit",
                target.name,
                limits.max_per_agent
            );
        }
        let next_run = fmt_time(schedule.first(ctx.now)?);
        let repeat = schedule.repeat();
        let id = db::insert_timer(
            ctx.conn,
            target.id,
            &me,
            &req.body,
            repeat.as_deref(),
            &next_run,
            &ctx.now_s,
        )?;
        tracing::info!(id, from = me.name, to = target.name, next_run, "set timer");
        Ok(proto::Timer {
            id,
            agent: target.name,
            creator: me.name,
            repeat,
            next_run,
            body: req.body,
        })
    })
}

/// An agent's timers, soonest first. Without an agent: an agent's own, or for
/// a human everyone's.
pub fn timer_list(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::TimerListRequest,
) -> Result<proto::TimerListResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let target = match req.agent.as_deref() {
            Some(name) => {
                let target = timer_agent(ctx, name)?;
                may_manage_timers(ctx, &me, &target)?;
                Some(target.id)
            }
            None if me.kind == db::Kind::Agent => Some(me.id),
            None => None,
        };
        let timers = db::timers(ctx.conn, target)?
            .into_iter()
            .map(timer_proto)
            .collect();
        Ok(proto::TimerListResponse { timers })
    })
}

pub fn timer_remove(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::TimerRemoveRequest,
) -> Result<proto::Timer> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let missing = || anyhow::anyhow!("no timer {}", req.id);
        let target_id = db::timer_target(ctx.conn, req.id)?.ok_or_else(missing)?;
        let timer = db::timers(ctx.conn, Some(target_id))?
            .into_iter()
            .find(|t| t.id == req.id)
            .ok_or_else(missing)?;
        let target = db::Principal {
            kind: db::Kind::Agent,
            id: target_id,
            name: timer.target.clone(),
        };
        may_manage_timers(ctx, &me, &target)?;
        db::delete_timer(ctx.conn, req.id)?;
        tracing::info!(id = req.id, by = me.name, "removed timer");
        Ok(timer_proto(timer))
    })
}

/// Timers fired in one transaction; more due wait for the next pass.
const FIRE_BATCH: u32 = 100;

/// Fire the timers due at `now`. Each sends its text to its agent as a direct
/// message from its creator and, in the same transaction, is rescheduled from
/// `now` (so missed runs are not caught up) or, if it ran once, deleted.
/// Returns how many fired and when the soonest timer left is due.
pub fn fire_timers(
    conn: &mut Connection,
    now: DateTime<Utc>,
) -> Result<(usize, Option<DateTime<Utc>>)> {
    let parse = |t: String| {
        DateTime::parse_from_rfc3339(&t)
            .map(|t| t.with_timezone(&Utc))
            .context("stored timer time")
    };
    // Most passes find nothing due: answer those without the write lock.
    let soonest = db::next_timer(conn)?.map(parse).transpose()?;
    if soonest.is_none_or(|t| t > now) {
        return Ok((0, soonest));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now_s = fmt_time(now);
    let due = db::due_timers(&tx, &now_s, FIRE_BATCH)?;
    let mut fired = 0;
    for timer in &due {
        // A stored schedule that no longer parses must not block the others.
        let schedule = match timer
            .repeat
            .as_deref()
            .map(Schedule::from_repeat)
            .transpose()
        {
            Ok(schedule) => schedule,
            Err(err) => {
                tracing::warn!(id = timer.id, "deleting timer with a bad schedule: {err:#}");
                db::delete_timer(&tx, timer.id)?;
                continue;
            }
        };
        let msg = db::insert_message(&tx, &timer.creator, None, &timer.body, &now_s)?;
        db::insert_delivery(&tx, msg, db::Kind::Agent, timer.target_id)?;
        db::set_message_timer(&tx, msg, timer.id)?;
        match schedule.and_then(|s| s.next_after(now)) {
            Some(next) => db::set_timer_next_run(&tx, timer.id, &fmt_time(next))?,
            None => db::delete_timer(&tx, timer.id)?,
        }
        fired += 1;
        tracing::info!(id = timer.id, from = timer.creator.name, "timer fired");
    }
    let next = db::next_timer(&tx)?.map(parse).transpose()?;
    tx.commit()?;
    Ok((fired, next))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A board with the human will, agents Mateo and Zed in @crew, and Lu in
    /// no group; the callers for each.
    fn board() -> (Connection, Caller, [Caller; 3]) {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let will = Caller {
            human: Some("will".into()),
            ..Default::default()
        };
        ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        group_register(
            &mut conn,
            &will,
            proto::GroupRegisterRequest {
                group: "crew".into(),
                description: None,
            },
        )
        .unwrap();
        let agents = ["Mateo", "Zed", "Lu"].map(|name| {
            let token = register(
                &mut conn,
                &will,
                proto::RegisterRequest {
                    name: Some(name.into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token;
            Caller {
                token: Some(token),
                ..Default::default()
            }
        });
        for agent in &agents[..2] {
            let req = proto::GroupRequest {
                group: "crew".into(),
            };
            group_join(&mut conn, agent, req).unwrap();
        }
        (conn, will, agents)
    }

    fn add(
        conn: &mut Connection,
        caller: &Caller,
        agent: &str,
        when: &str,
    ) -> Result<proto::Timer> {
        let req = proto::TimerAddRequest {
            agent: agent.into(),
            when: when.into(),
            body: format!("{when} for {agent}"),
        };
        timer_add(conn, caller, req, TimerLimits::default())
    }

    fn inbox(conn: &mut Connection, caller: &Caller) -> Vec<proto::Message> {
        read(conn, caller, proto::ReadRequest::default())
            .unwrap()
            .messages
    }

    fn time(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn effort_reports_validate_and_round_trip() {
        let (mut conn, will, agents) = board();
        let mateo = agents[0].clone();
        let zed = agents[1].clone();
        let report = |pending: Option<proto::EffortRequest>,
                      answered: Option<proto::EffortOutcome>| proto::ReportEffortRequest {
            report: Some(proto::EffortReport {
                state: Some(proto::EffortState {
                    supported: true,
                    reason: String::new(),
                    choices: vec![
                        proto::EffortChoice {
                            value: "low".into(),
                            label: "Low".into(),
                            rank: Some(1),
                        },
                        proto::EffortChoice {
                            value: "high".into(),
                            label: "High".into(),
                            rank: Some(2),
                        },
                    ],
                    current: Some("low".into()),
                }),
                pending,
                answered,
            }),
        };
        // Humans cannot report.
        assert!(report_effort(&mut conn, &will, report(None, None)).is_err());
        // A valid report round-trips through the list and the summaries.
        report_effort(&mut conn, &mateo, report(None, None)).unwrap();
        let agents = list_agents(&mut conn, &will, proto::ListAgentsRequest { all: false })
            .unwrap()
            .agents;
        let m = agents.iter().find(|a| a.name == "Mateo").unwrap();
        let e = m.effort.as_ref().unwrap();
        assert_eq!(e.state.as_ref().unwrap().current.as_deref(), Some("low"));
        assert_eq!(e.state.as_ref().unwrap().choices.len(), 2);
        assert!(m.effort_reported_at.is_some());
        let summaries = agent_summary(&mut conn, &will, proto::AgentSummaryRequest {})
            .unwrap()
            .agents;
        assert!(
            summaries
                .iter()
                .find(|s| s.agent == "Mateo")
                .unwrap()
                .effort
                .is_some()
        );
        // Duplicate choice values, unspecified outcomes and bad ids rejected.
        let mut bad = report(None, None);
        bad.report
            .as_mut()
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .choices
            .push(proto::EffortChoice {
                value: "low".into(),
                label: "Also low".into(),
                rank: None,
            });
        let err = report_effort(&mut conn, &mateo, bad).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate"), "{err:#}");
        let bad = report(
            None,
            Some(proto::EffortOutcome {
                request_id: 1,
                result: 0,
                detail: String::new(),
            }),
        );
        let err = report_effort(&mut conn, &mateo, bad).unwrap_err();
        assert!(format!("{err:#}").contains("unspecified"), "{err:#}");
        let bad = report(
            Some(proto::EffortRequest {
                request_id: 0,
                value: "high".into(),
            }),
            None,
        );
        assert!(report_effort(&mut conn, &mateo, bad).is_err());
        // A request id must name a set-effort control delivered to this agent.
        let bad = report(
            Some(proto::EffortRequest {
                request_id: 42,
                value: "high".into(),
            }),
            None,
        );
        let err = report_effort(&mut conn, &mateo, bad).unwrap_err();
        assert!(format!("{err:#}").contains("not a set-effort control"), "{err:#}");
        // A real control's id validates, and the send returns it.
        let sent = send(
            &mut conn,
            &will,
            proto::SendRequest {
                to: "Mateo".into(),
                body: "high".into(),
                control: Some(proto::Control::SetEffort.into()),
            },
        )
        .unwrap();
        let id = sent.id.unwrap();
        report_effort(
            &mut conn,
            &mateo,
            report(
                Some(proto::EffortRequest {
                    request_id: id,
                    value: "high".into(),
                }),
                None,
            ),
        )
        .unwrap();
        // ...but not to another agent.
        let err = report_effort(
            &mut conn,
            &zed,
            report(
                Some(proto::EffortRequest {
                    request_id: id,
                    value: "high".into(),
                }),
                None,
            ),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a set-effort control"), "{err:#}");
        // The control reads back with its kind, and retirement clears reports.
        let mail = read(&mut conn, &mateo, proto::ReadRequest::default())
            .unwrap()
            .messages;
        let control = mail.iter().find(|m| m.control.is_some()).unwrap();
        assert_eq!(control.control, Some(proto::Control::SetEffort.into()));
        // An answered outcome round-trips and clears nothing else.
        report_effort(
            &mut conn,
            &mateo,
            report(
                None,
                Some(proto::EffortOutcome {
                    request_id: id,
                    result: proto::effort_outcome::Result::Applied as i32,
                    detail: String::new(),
                }),
            ),
        )
        .unwrap();
        let agents = list_agents(&mut conn, &will, proto::ListAgentsRequest { all: false })
            .unwrap()
            .agents;
        let e = agents
            .iter()
            .find(|a| a.name == "Mateo")
            .unwrap()
            .effort
            .clone()
            .unwrap();
        let answered = e.answered.unwrap();
        assert_eq!(answered.request_id, id);
        assert_eq!(
            answered.result(),
            proto::effort_outcome::Result::Applied
        );
        // A request cannot be pending and answered in one report.
        let both = report(
            Some(proto::EffortRequest {
                request_id: id,
                value: "high".into(),
            }),
            Some(proto::EffortOutcome {
                request_id: id,
                result: proto::effort_outcome::Result::Applied as i32,
                detail: String::new(),
            }),
        );
        let err = report_effort(&mut conn, &mateo, both).unwrap_err();
        assert!(format!("{err:#}").contains("both pending and answered"), "{err:#}");
        // Pending values must match the control's body exactly.
        let mismatched = report(
            Some(proto::EffortRequest {
                request_id: id,
                value: "low".into(),
            }),
            None,
        );
        let err = report_effort(&mut conn, &mateo, mismatched).unwrap_err();
        assert!(format!("{err:#}").contains("does not match"), "{err:#}");
        // Empty pending values, empty or oversized control values rejected.
        let empty_pending = report(
            Some(proto::EffortRequest {
                request_id: id,
                value: String::new(),
            }),
            None,
        );
        let err = report_effort(&mut conn, &mateo, empty_pending).unwrap_err();
        assert!(format!("{err:#}").contains("without a value"), "{err:#}");
        let send_effort = |conn: &mut Connection, body: &str| {
            send(
                conn,
                &will,
                proto::SendRequest {
                    to: "Mateo".into(),
                    body: body.into(),
                    control: Some(proto::Control::SetEffort.into()),
                },
            )
        };
        assert!(send_effort(&mut conn, "").is_err());
        assert!(send_effort(&mut conn, &"x".repeat(129)).is_err());
        // A report without state is rejected.
        let err = report_effort(
            &mut conn,
            &mateo,
            proto::ReportEffortRequest { report: None },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("missing report"), "{err:#}");
        let mut no_state = report(None, None);
        no_state.report.as_mut().unwrap().state = None;
        let err = report_effort(&mut conn, &mateo, no_state).unwrap_err();
        assert!(format!("{err:#}").contains("state"), "{err:#}");
        bye(&mut conn, &mateo).unwrap();
        let agents = list_agents(&mut conn, &will, proto::ListAgentsRequest { all: true })
            .unwrap()
            .agents;
        let m = agents.iter().find(|a| a.name == "Mateo").unwrap();
        assert!(m.effort.is_none());
    }

    #[test]
    fn last_sent_at_tracks_sends_and_activity_not_reads_or_presence() {
        let (mut conn, will, agents) = board();
        let sent = |conn: &mut Connection| {
            list_agents(conn, &will, proto::ListAgentsRequest { all: false })
                .unwrap()
                .agents
                .into_iter()
                .map(|a| (a.name, a.last_sent_at))
                .collect::<Vec<_>>()
        };
        let at = |rows: &[(String, Option<String>)], name: &str| {
            rows.iter().find(|(n, _)| n == name).unwrap().1.clone()
        };
        // Lu never sent and joined no group: absent. Mateo and Zed have their
        // join announcements, which are messages they sent.
        let rows = sent(&mut conn);
        assert!(at(&rows, "Lu").is_none());
        let mateo_join = at(&rows, "Mateo").unwrap();
        let zed_join = at(&rows, "Zed").unwrap();
        send(
            &mut conn,
            &agents[0],
            proto::SendRequest {
                to: "Zed".into(),
                body: "hi".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let rows = sent(&mut conn);
        let mateo_at = at(&rows, "Mateo").unwrap();
        assert!(mateo_at >= mateo_join);
        assert_eq!(at(&rows, "Zed").as_deref(), Some(zed_join.as_str()));
        // Mateo's stamp matches the stored message's server time.
        let stored: String = conn
            .query_row(
                "SELECT created_at FROM messages WHERE from_kind = 'agent' ORDER BY id DESC",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mateo_at, stored);
        // Reads and presence touches do not move it.
        whoami(&mut conn, &agents[0]).unwrap();
        inbox(&mut conn, &agents[0]);
        inbox(&mut conn, &agents[1]);
        let rows = sent(&mut conn);
        assert_eq!(at(&rows, "Mateo").as_deref(), Some(mateo_at.as_str()));
        assert_eq!(at(&rows, "Zed").as_deref(), Some(zed_join.as_str()));
        // Posting activity does.
        post_activity(
            &mut conn,
            &agents[1],
            vec![proto::ActivityEvent {
                record: r#"{"event":"text","text":"working"}"#.into(),
            }],
        )
        .unwrap();
        let rows = sent(&mut conn);
        let zed_at = at(&rows, "Zed").unwrap();
        assert!(zed_at >= zed_join);
        assert_eq!(at(&rows, "Mateo").as_deref(), Some(mateo_at.as_str()));
    }

    #[test]
    fn human_conversations_count_direct_mail_either_way_and_nothing_else() {
        let (mut conn, will, [mateo, zed, _lu]) = board();
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        let ada = Caller {
            human: Some("ada".into()),
            ..Default::default()
        };
        let dm = |conn: &mut Connection, from: &Caller, to: &str| {
            let req = proto::SendRequest {
                to: to.into(),
                body: "hi".into(),
                ..Default::default()
            };
            send(conn, from, req).unwrap().id.unwrap()
        };
        let talked = |conn: &mut Connection| {
            let mut rows: Vec<_> = list_agents(conn, &will, proto::ListAgentsRequest { all: true })
                .unwrap()
                .agents
                .into_iter()
                .map(|a| (a.name, a.last_human_conversation_at))
                .collect();
            rows.sort();
            rows
        };
        let none = |names: &[&str]| -> Vec<(String, Option<String>)> {
            names.iter().map(|n| (n.to_string(), None)).collect()
        };
        // Group joins and broadcasts, agent-to-agent mail, a control and a
        // timer message from a human are not conversations with a human.
        let req = proto::SendRequest {
            to: "@crew".into(),
            body: "all hands".into(),
            ..Default::default()
        };
        send(&mut conn, &will, req).unwrap();
        dm(&mut conn, &mateo, "Zed");
        let req = proto::SendRequest {
            to: "Lu".into(),
            body: "stop".into(),
            control: Some(proto::Control::Cancel.into()),
        };
        send(&mut conn, &will, req).unwrap();
        let t = add(&mut conn, &will, "Lu", "in 1m").unwrap();
        assert_eq!(fire_timers(&mut conn, time(&t.next_run)).unwrap().0, 1);
        assert_eq!(talked(&mut conn), none(&["Lu", "Mateo", "Zed"]));
        // A human to an agent, and an agent to a human, each count once, at
        // the message's server time, for that pair only.
        let to_mateo = dm(&mut conn, &will, "Mateo");
        let to_ada = dm(&mut conn, &zed, "ada");
        let created = |conn: &mut Connection, id: i64| -> String {
            conn.query_row("SELECT created_at FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let (mateo_at, zed_at) = (created(&mut conn, to_mateo), created(&mut conn, to_ada));
        assert_eq!(
            talked(&mut conn),
            [
                ("Lu".to_string(), None),
                ("Mateo".to_string(), Some(mateo_at.clone())),
                ("Zed".to_string(), Some(zed_at.clone())),
            ]
        );
        let pairs: Vec<(String, String)> = conn
            .prepare(
                "SELECT a.name, h.name FROM agent_human_conversations p
                 JOIN agents a ON a.id = p.agent_id JOIN humans h ON h.id = p.human_id
                 ORDER BY a.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            pairs,
            [
                ("Mateo".into(), "will".into()),
                ("Zed".into(), "ada".into())
            ]
        );
        // Any human's conversation counts, so every human's summary agrees.
        for human in [&will, &ada] {
            let mut rows: Vec<_> = agent_summary(&mut conn, human, proto::AgentSummaryRequest {})
                .unwrap()
                .agents
                .into_iter()
                .map(|s| (s.agent, s.last_human_conversation_at))
                .collect();
            rows.sort();
            assert_eq!(rows, talked(&mut conn));
        }
        // A failed send records nothing.
        let req = proto::SendRequest {
            to: "Nobody".into(),
            body: "hi".into(),
            ..Default::default()
        };
        assert!(send(&mut conn, &will, req).is_err());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM agent_human_conversations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn a_one_time_timer_sends_once_from_its_creator() {
        let (mut conn, _, [mateo, zed, _]) = board();
        let t = add(&mut conn, &mateo, "zed", "in 30m").unwrap();
        assert_eq!((t.agent.as_str(), t.creator.as_str()), ("Zed", "Mateo"));
        assert_eq!(t.repeat, None);
        let due = time(&t.next_run);
        // Not yet due: nothing fires, and the next run is reported.
        let early = due - chrono::Duration::seconds(1);
        assert_eq!(fire_timers(&mut conn, early).unwrap(), (0, Some(due)));
        assert_eq!(fire_timers(&mut conn, due).unwrap(), (1, None));
        let mail: Vec<_> = inbox(&mut conn, &zed)
            .into_iter()
            .filter(|m| m.timer_id.is_some())
            .collect();
        assert_eq!(mail.len(), 1);
        assert_eq!(
            (
                mail[0].from_name.as_str(),
                mail[0].body.as_str(),
                mail[0].timer_id
            ),
            ("Mateo", "in 30m for zed", Some(t.id))
        );
        // Run once, it is gone.
        assert_eq!(fire_timers(&mut conn, due).unwrap(), (0, None));
        let listed = timer_list(&mut conn, &zed, proto::TimerListRequest::default()).unwrap();
        assert!(listed.timers.is_empty());
    }

    #[test]
    fn a_recurring_timer_missed_for_a_day_fires_once_then_resumes_from_now() {
        let (mut conn, _, [mateo, _, _]) = board();
        // An agent may always set its own.
        let t = add(&mut conn, &mateo, "Mateo", "every 1h").unwrap();
        assert_eq!(t.repeat.as_deref(), Some("every 1h"));
        let late = time(&t.next_run) + chrono::Duration::days(1);
        let (fired, next) = fire_timers(&mut conn, late).unwrap();
        assert_eq!((fired, next), (1, Some(late + chrono::Duration::hours(1))));
        let fired: Vec<_> = inbox(&mut conn, &mateo)
            .into_iter()
            .filter_map(|m| m.timer_id)
            .collect();
        assert_eq!(fired, [t.id]);
        let listed = timer_list(&mut conn, &mateo, proto::TimerListRequest::default()).unwrap();
        assert_eq!(
            listed.timers[0].next_run,
            fmt_time(late + chrono::Duration::hours(1))
        );
    }

    #[test]
    fn a_timer_whose_schedule_no_longer_parses_is_deleted_without_blocking_others() {
        let (mut conn, _, [mateo, zed, _]) = board();
        let bad = add(&mut conn, &mateo, "Zed", "every 1h").unwrap();
        let good = add(&mut conn, &mateo, "Zed", "in 1h").unwrap();
        conn.execute(
            "UPDATE timers SET repeat = 'every fortnight-ish' WHERE id = ?1",
            [bad.id],
        )
        .unwrap();
        let due = time(&good.next_run).max(time(&bad.next_run));
        assert_eq!(fire_timers(&mut conn, due).unwrap(), (1, None));
        let fired: Vec<_> = inbox(&mut conn, &zed)
            .into_iter()
            .filter_map(|m| m.timer_id)
            .collect();
        assert_eq!(fired, [good.id]);
        assert!(db::timers(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn limits_apply_to_the_target() {
        let (mut conn, will, [mateo, _, _]) = board();
        let short = add(&mut conn, &will, "Zed", "every 1m").unwrap_err();
        assert!(
            format!("{short:#}").contains("shortest interval is 5m"),
            "{short:#}"
        );
        // Mostly hourly, but two runs a minute apart.
        let uneven = add(&mut conn, &will, "Zed", "0,1 * * * *").unwrap_err();
        assert!(
            format!("{uneven:#}").contains("as often as every 1m"),
            "{uneven:#}"
        );
        // One-time timers have no interval to limit.
        add(&mut conn, &will, "Zed", "in 1m").unwrap();
        let limits = TimerLimits {
            max_per_agent: 2,
            ..TimerLimits::default()
        };
        let req = |when: &str| proto::TimerAddRequest {
            agent: "Zed".into(),
            when: when.into(),
            body: "x".into(),
        };
        timer_add(&mut conn, &mateo, req("in 2h"), limits).unwrap();
        let full = timer_add(&mut conn, &mateo, req("in 3h"), limits).unwrap_err();
        assert!(
            format!("{full:#}").contains("Zed already has 2 timers"),
            "{full:#}"
        );
    }

    #[test]
    fn agents_manage_timers_only_for_agents_they_share_a_group_with() {
        let (mut conn, will, [mateo, _, lu]) = board();
        let no = add(&mut conn, &lu, "Zed", "in 1h").unwrap_err();
        assert!(
            format!("{no:#}").contains("Zed is not in any of your groups"),
            "{no:#}"
        );
        let t = add(&mut conn, &mateo, "Zed", "in 1h").unwrap();
        let rm = |conn: &mut Connection, caller: &Caller| {
            timer_remove(conn, caller, proto::TimerRemoveRequest { id: t.id })
        };
        let list = |conn: &mut Connection, caller: &Caller| {
            let req = proto::TimerListRequest {
                agent: Some("Zed".into()),
            };
            timer_list(conn, caller, req)
        };
        assert!(rm(&mut conn, &lu).is_err());
        assert!(list(&mut conn, &lu).is_err());
        // Humans may, as with messages.
        assert_eq!(list(&mut conn, &will).unwrap().timers.len(), 1);
        let everyone = timer_list(&mut conn, &will, proto::TimerListRequest::default()).unwrap();
        assert_eq!(everyone.timers.len(), 1);
        assert_eq!(rm(&mut conn, &will).unwrap().id, t.id);
        let gone = rm(&mut conn, &mateo).unwrap_err();
        assert!(
            format!("{gone:#}").contains(&format!("no timer {}", t.id)),
            "{gone:#}"
        );
    }

    #[test]
    fn an_agent_in_no_group_manages_its_own_timers() {
        let (mut conn, _, [_, _, lu]) = board();
        let t = add(&mut conn, &lu, "Lu", "in 1h").unwrap();
        let req = proto::TimerListRequest {
            agent: Some("Lu".into()),
        };
        assert_eq!(timer_list(&mut conn, &lu, req).unwrap().timers.len(), 1);
        timer_remove(&mut conn, &lu, proto::TimerRemoveRequest { id: t.id }).unwrap();
    }

    #[test]
    fn agents_message_any_human_who_sees_it_in_their_feed_and_marks_it_read() {
        let (mut conn, will, [mateo, zed, lu]) = board();
        // Lu shares no group with anyone, and needs none to reach a human.
        for (agent, body) in [(&lu, "from lu"), (&mateo, "from mateo")] {
            let req = proto::SendRequest {
                to: "will".into(),
                body: body.into(),
                control: None,
            };
            send(&mut conn, agent, req).unwrap();
        }
        let will_id = db::find_human(&conn, "will").unwrap().unwrap().id;
        let lu_id = db::live_agent_by_name(&conn, "Lu").unwrap().unwrap().id;
        let scope = db::FeedFilter {
            scope: Some(will_id),
            ..Default::default()
        };
        let feed = db::feed_messages(&conn, 10, &scope).unwrap();
        assert!(
            feed.iter()
                .any(|m| m.body == "from lu" && m.to_name.as_deref() == Some("will"))
        );
        let with = db::FeedFilter {
            with: Some(vec![(db::Kind::Agent, lu_id)]),
            ..Default::default()
        };
        let thread = db::feed_messages(&conn, 10, &with).unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].read_at, None);

        let mark = |conn: &mut Connection, from: &str| {
            let req = proto::MarkReadRequest { from: from.into() };
            mark_read(conn, &will, req).unwrap().count
        };
        assert_eq!(mark(&mut conn, "lu"), 1);
        // Already read: nothing changes, so the server signals nothing.
        assert_eq!(mark(&mut conn, "lu"), 0);
        assert!(
            db::feed_messages(&conn, 10, &with).unwrap()[0]
                .read_at
                .is_some()
        );
        // Only Lu's mail: Mateo's is still unread.
        let unread: Vec<_> = read(&mut conn, &will, proto::ReadRequest::default())
            .unwrap()
            .messages
            .into_iter()
            .filter(|m| m.to_group.is_none())
            .map(|m| m.body)
            .collect();
        assert_eq!(unread, ["from mateo"]);
        // An unknown sender is an error.
        let req = proto::MarkReadRequest {
            from: "nobody".into(),
        };
        let err = mark_read(&mut conn, &zed, req).unwrap_err();
        assert!(
            format!("{err:#}").contains("no such agent or human: nobody"),
            "{err:#}"
        );
    }

    #[test]
    fn summaries_count_unread_direct_mail_and_unseen_activity_per_agent() {
        let (mut conn, will, [mateo, zed, _]) = board();
        let summary = |conn: &mut Connection| {
            agent_summary(conn, &will, proto::AgentSummaryRequest {})
                .unwrap()
                .agents
        };
        let find = |s: &[proto::AgentSummary], name: &str| {
            s.iter().find(|s| s.agent == name).unwrap().clone()
        };
        // Every live agent is listed, with zeros when nothing is unread or
        // unseen. Join notices are broadcasts, which never count as unread;
        // they are messages the joining agent sent, so they set last_sent_at.
        let s = summary(&mut conn);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s.iter().all(|s| s.unread_messages == 0 && s.unseen_stream == 0));
        assert!(find(&s, "Lu").last_sent_at.is_none());
        assert!(find(&s, "Mateo").last_sent_at.is_some());
        let dm = proto::SendRequest {
            to: "will".into(),
            body: "a question".into(),
            control: None,
        };
        send(&mut conn, &mateo, dm).unwrap();
        let shout = proto::SendRequest {
            to: "@crew".into(),
            body: "everyone".into(),
            control: None,
        };
        send(&mut conn, &will, shout).unwrap();
        let record = |n: usize| {
            (0..n)
                .map(|i| proto::ActivityEvent {
                    record: format!(r#"{{"ts":"2026-10-16T12:00:0{i}Z","event":"text"}}"#),
                })
                .collect::<Vec<_>>()
        };
        post_activity(&mut conn, &zed, record(3)).unwrap();
        let s = summary(&mut conn);
        assert_eq!(s.len(), 3, "{s:?}");
        let mateo_s = find(&s, "Mateo");
        assert_eq!((mateo_s.unread_messages, mateo_s.unseen_stream), (1, 0));
        assert!(mateo_s.oldest_unread_message.is_some());
        let zed_s = find(&s, "Zed");
        assert_eq!((zed_s.unread_messages, zed_s.unseen_stream), (0, 3));
        assert!(zed_s.oldest_unseen_stream.is_some());
        assert!(zed_s.last_sent_at.is_some());

        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM activity ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let seen = |conn: &mut Connection, through_id: i64| {
            let req = proto::MarkSeenRequest {
                agent: "zed".into(),
                through_id,
            };
            mark_seen(conn, &will, req).unwrap().moved
        };
        assert!(seen(&mut conn, ids[1]));
        assert_eq!(find(&summary(&mut conn), "Zed").unseen_stream, 1);
        // Never back, and never past the newest record, so later ones stay unseen.
        assert!(!seen(&mut conn, ids[0]));
        assert!(seen(&mut conn, i64::MAX));
        assert!(!seen(&mut conn, i64::MAX));
        post_activity(&mut conn, &zed, record(1)).unwrap();
        assert_eq!(find(&summary(&mut conn), "Zed").unseen_stream, 1);

        // Reading clears unread mail; agents get no summary.
        let req = proto::MarkReadRequest {
            from: "Mateo".into(),
        };
        mark_read(&mut conn, &will, req).unwrap();
        let s = summary(&mut conn);
        assert_eq!(find(&s, "Mateo").unread_messages, 0);
        assert!(agent_summary(&mut conn, &zed, proto::AgentSummaryRequest {}).is_err());
        // Retiring removes the agent and its cursors.
        bye(&mut conn, &zed).unwrap();
        let s = summary(&mut conn);
        assert_eq!(
            s.iter().map(|s| s.agent.as_str()).collect::<Vec<_>>(),
            ["Lu", "Mateo"]
        );
        let cursors: i64 = conn
            .query_row("SELECT count(*) FROM activity_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cursors, 0);
    }

    #[test]
    fn retiring_an_agent_deletes_its_timers() {
        let (mut conn, will, [mateo, zed, _]) = board();
        add(&mut conn, &mateo, "Zed", "every 1h").unwrap();
        add(&mut conn, &zed, "Mateo", "every 1h").unwrap();
        bye(&mut conn, &zed).unwrap();
        let left = timer_list(&mut conn, &will, proto::TimerListRequest::default()).unwrap();
        let agents: Vec<_> = left.timers.iter().map(|t| t.agent.as_str()).collect();
        // Zed's timers went; the one Zed set for Mateo stays.
        assert_eq!(agents, ["Mateo"]);
        let retired = add(&mut conn, &mateo, "Zed", "in 1h").unwrap_err();
        assert!(
            format!("{retired:#}").contains("Zed is retired"),
            "{retired:#}"
        );
    }
}
