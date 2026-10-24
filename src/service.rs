//! Business logic: one function per command over a SQLite connection, taking
//! and returning the protobuf types. Each runs in its own `IMMEDIATE`
//! transaction, generates `now`, and resolves the caller. It knows nothing
//! about sockets, files or output formatting.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
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

mod blockers;
pub use blockers::{answer_blocker, cancel_blocker, get_blocker, list_blockers, put_blocker};
mod notes;
pub use notes::{MAX_NOTE_BYTES, list_agent_notes, put_agent_note};

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
        owner: None,
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
        auto_reply: if m.auto_reply { Some(true) } else { None },
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
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        let owner = match me.kind {
            db::Kind::Agent => db::agent_owner_name(ctx.conn, me.id)?,
            db::Kind::Human => None,
        };
        Ok(proto::Principal {
            owner,
            ..principal(me)
        })
    })
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

// ---- side channels (agent-interactions.v1) -------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct InputOptionJson {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InputQuestionJson {
    id: String,
    header: String,
    text: String,
    options: Vec<InputOptionJson>,
    allow_free_text: bool,
    required: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InputAnswerJson {
    question_id: String,
    value: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InputFormJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    is_blocking: bool,
    questions: Vec<InputQuestionJson>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UsageJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    used_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_tokens: Option<u64>,
    source: String,
}

fn question_json(q: &proto::InputQuestion) -> InputQuestionJson {
    InputQuestionJson {
        id: q.id.clone(),
        header: q.header.clone(),
        text: q.text.clone(),
        options: q
            .options
            .iter()
            .map(|o| InputOptionJson {
                value: o.value.clone(),
                label: o.label.clone(),
                description: o.description.clone(),
            })
            .collect(),
        allow_free_text: q.allow_free_text,
        required: q.required,
    }
}

fn question_proto(q: InputQuestionJson) -> proto::InputQuestion {
    proto::InputQuestion {
        id: q.id,
        header: q.header,
        text: q.text,
        options: q
            .options
            .into_iter()
            .map(|o| proto::InputOption {
                value: o.value,
                label: o.label,
                description: o.description,
            })
            .collect(),
        allow_free_text: q.allow_free_text,
        required: q.required,
    }
}

fn answer_json(a: &proto::InputAnswer) -> InputAnswerJson {
    InputAnswerJson {
        question_id: a.question_id.clone(),
        value: a.value.clone(),
    }
}

fn answer_proto(a: InputAnswerJson) -> proto::InputAnswer {
    proto::InputAnswer {
        question_id: a.question_id,
        value: a.value,
    }
}

fn usage_proto(usage: Option<String>) -> Result<Option<proto::ContextUsage>> {
    let Some(json) = usage else { return Ok(None) };
    let parsed: UsageJson = serde_json::from_str(&json).context("invalid stored context usage")?;
    let source = match parsed.source.as_str() {
        "current" => proto::context_usage::Source::CurrentContext,
        _ => proto::context_usage::Source::LastModelRequest,
    };
    Ok(Some(proto::ContextUsage {
        used_tokens: parsed.used_tokens,
        limit_tokens: parsed.limit_tokens,
        source: source.into(),
    }))
}

fn validate_generation(ctx: &Ctx, agent: &db::Principal, generation: &str) -> Result<()> {
    if generation.is_empty() || generation.len() > 128 {
        bail!("generation must be 1 to 128 bytes");
    }
    if !db::check_generation(ctx.conn, agent.id, generation)? {
        bail!("stale side generation; begin a new side session");
    }
    Ok(())
}

/// A fresh interaction session: same-generation is a no-op (lost ACK safe),
/// a new generation resets pending input and usage.
pub fn begin_side_session(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::BeginSideSessionRequest,
) -> Result<proto::BeginSideSessionResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        if req.generation.is_empty() || req.generation.len() > 128 {
            bail!("generation must be 1 to 128 bytes");
        }
        db::begin_side_session(ctx.conn, me.id, &req.generation, &ctx.now_s)?;
        Ok(proto::BeginSideSessionResponse {
            generation: req.generation,
        })
    })
}

/// Latest context usage and capability; replaces or clears usage explicitly.
pub fn report_context(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReportContextRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        validate_generation(ctx, &me, &req.generation)?;
        let usage_json = match &req.usage {
            None => None,
            Some(usage) => {
                if usage.source() == proto::context_usage::Source::Unspecified {
                    bail!("usage source is required when usage is reported");
                }
                if usage.limit_tokens == Some(0) {
                    bail!("usage limit must be positive when present");
                }
                let source = match usage.source() {
                    proto::context_usage::Source::CurrentContext => "current",
                    _ => "request",
                };
                Some(serde_json::to_string(&UsageJson {
                    used_tokens: usage.used_tokens,
                    limit_tokens: usage.limit_tokens,
                    source: source.into(),
                })?)
            }
        };
        if req
            .side_reason
            .as_deref()
            .is_some_and(|r| r.len() > 4 * 1024)
        {
            bail!("side reason exceeds 4 KiB");
        }
        if req.preserve_usage && req.usage.is_some() {
            bail!("preserve_usage does not take new usage");
        }
        db::report_context(
            ctx.conn,
            me.id,
            usage_json.as_deref(),
            db::SideCapability {
                supported: req.side_supported,
                reason: req.side_reason.as_deref(),
                followups: req.side_followups,
            },
            &ctx.now_s,
            req.preserve_usage,
        )?;
        Ok(proto::Empty {})
    })
}

const MAX_QUESTIONS: usize = 3;
const MAX_OPTIONS: usize = 8;
const MAX_INPUT_TOTAL: usize = 16 * 1024;
const MAX_LABEL: usize = 256;
const MAX_DESCRIPTION: usize = 2 * 1024;

/// A structured question set for the owning human; one pending per agent.
/// `request_seq` makes a lost publish ACK safe: the same sequence with the
/// same form returns the original id; anything else at or below it is rejected.
pub fn request_owner_input(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::RequestOwnerInputRequest,
) -> Result<proto::RequestOwnerInputResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        validate_generation(ctx, &me, &req.generation)?;
        if db::agent_owner_id(ctx.conn, me.id)?.is_none() {
            bail!("{} has no registered owner", me.name);
        }
        if req.request_seq < 1 {
            bail!("request sequence must be positive");
        }
        let state = db::side_state(ctx.conn, me.id)?
            .ok_or_else(|| anyhow!("begin a side session first"))?;
        if req.request_seq <= state.closed_seq {
            bail!("request sequence is closed");
        }
        if req.questions.is_empty() || req.questions.len() > MAX_QUESTIONS {
            bail!("ask 1 to {MAX_QUESTIONS} questions");
        }
        let mut ids = std::collections::HashSet::new();
        for q in &req.questions {
            if q.id.is_empty() || q.id.len() > 128 || !ids.insert(q.id.clone()) {
                bail!("question ids must be unique, 1 to 128 bytes");
            }
            if q.header.len() > 128 {
                bail!("question header exceeds 128 bytes");
            }
            if q.text.is_empty() || q.text.len() > 4 * 1024 {
                bail!("question text must be 1 byte to 4 KiB");
            }
            if q.options.is_empty() && !q.allow_free_text {
                bail!("question {} offers no way to answer", q.id);
            }
            if q.options.len() > MAX_OPTIONS {
                bail!("at most {MAX_OPTIONS} options per question");
            }
            let mut values = std::collections::HashSet::new();
            for o in &q.options {
                if o.value.is_empty() || o.value.len() > 128 || !values.insert(o.value.clone()) {
                    bail!("option values must be unique, 1 to 128 bytes");
                }
                if o.label.as_deref().is_some_and(|l| l.len() > MAX_LABEL) {
                    bail!("option label exceeds {MAX_LABEL} bytes");
                }
                if o.description
                    .as_deref()
                    .is_some_and(|d| d.len() > MAX_DESCRIPTION)
                {
                    bail!("option description exceeds {} KiB", MAX_DESCRIPTION / 1024);
                }
            }
        }
        if req.message.as_deref().is_some_and(|m| m.len() > 4 * 1024) {
            bail!("form message exceeds 4 KiB");
        }
        let form = serde_json::to_string(&InputFormJson {
            message: req.message.clone(),
            is_blocking: req.is_blocking,
            questions: req.questions.iter().map(question_json).collect(),
        })?;
        if form.len() > MAX_INPUT_TOTAL {
            bail!("question form exceeds {} KiB", MAX_INPUT_TOTAL / 1024);
        }
        if let Some(latest) = db::latest_owner_input(ctx.conn, me.id)?
            && latest.generation == req.generation
        {
            if req.request_seq < latest.seq {
                bail!("stale request sequence");
            }
            if req.request_seq == latest.seq {
                if latest.form != form {
                    bail!("request sequence reused with a different form");
                }
                // A retry after a lost publish reply: the same request.
                return Ok(proto::RequestOwnerInputResponse {
                    request_id: latest.id,
                });
            }
            if latest.status == "pending" {
                bail!("a question set is already pending");
            }
        }
        let id = db::insert_owner_input(
            ctx.conn,
            me.id,
            &req.generation,
            req.request_seq,
            &form,
            &ctx.now_s,
        )?;
        Ok(proto::RequestOwnerInputResponse { request_id: id })
    })
}

fn input_status_proto(status: &str) -> proto::InputStatus {
    match status {
        "answered" => proto::InputStatus::Answered,
        "declined" => proto::InputStatus::Declined,
        "cancelled" => proto::InputStatus::Cancelled,
        "stale" => proto::InputStatus::Stale,
        _ => proto::InputStatus::Pending,
    }
}

fn owner_input_proto(row: db::OwnerInputRow) -> Result<proto::OwnerInput> {
    let form: InputFormJson =
        serde_json::from_str(&row.form).context("invalid stored question form")?;
    let answers: Vec<InputAnswerJson> = match &row.answers {
        Some(a) => serde_json::from_str(a).context("invalid stored answers")?,
        None => Vec::new(),
    };
    Ok(proto::OwnerInput {
        request_id: row.id,
        status: input_status_proto(&row.status).into(),
        questions: form.questions.into_iter().map(question_proto).collect(),
        answers: answers.into_iter().map(answer_proto).collect(),
        updated_at: row.updated_at,
        message: form.message,
        is_blocking: form.is_blocking,
    })
}

/// The owner's answer, decline or cancel; exact request id, exact question ids.
pub fn answer_owner_input(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AnswerOwnerInputRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let Some(row) = db::owner_input(ctx.conn, req.request_id)? else {
            bail!("no such input request");
        };
        let agent_id = row_agent(ctx.conn, req.request_id)?;
        if !db::agent_is_live(ctx.conn, agent_id)? {
            bail!("the agent is retired");
        }
        if db::agent_owner_id(ctx.conn, agent_id)? != Some(me.id) {
            bail!("not your agent's question");
        }
        if row.status != "pending" {
            bail!("input request is not pending");
        }
        let form: InputFormJson =
            serde_json::from_str(&row.form).context("invalid stored question form")?;
        let questions = &form.questions;
        let (status, answers) = match req.action() {
            proto::AnswerAction::Submit => {
                let mut seen = std::collections::HashSet::new();
                for a in &req.answers {
                    if a.value.len() > 4 * 1024 {
                        bail!("answer exceeds 4 KiB");
                    }
                    let Some(q) = questions.iter().find(|q| q.id == a.question_id) else {
                        bail!("answer names an unknown question");
                    };
                    if !seen.insert(a.question_id.clone()) {
                        bail!("duplicate answer for one question");
                    }
                    if !q.allow_free_text
                        && !q.options.is_empty()
                        && !q.options.iter().any(|o| o.value == a.value)
                    {
                        bail!("answer is not an offered option");
                    }
                }
                for q in questions {
                    let answered = req
                        .answers
                        .iter()
                        .any(|a| a.question_id == q.id && !a.value.is_empty());
                    if q.required && !answered {
                        bail!("required question unanswered: {}", q.id);
                    }
                }
                (
                    "answered",
                    Some(serde_json::to_string(
                        &req.answers.iter().map(answer_json).collect::<Vec<_>>(),
                    )?),
                )
            }
            proto::AnswerAction::Decline | proto::AnswerAction::Cancel => {
                if !req.answers.is_empty() {
                    bail!("answers only accompany a submit");
                }
                (
                    if req.action() == proto::AnswerAction::Decline {
                        "declined"
                    } else {
                        "cancelled"
                    },
                    None,
                )
            }
            _ => bail!("an answer action is required"),
        };
        db::finish_owner_input(ctx.conn, row.id, answers.as_deref(), status, &ctx.now_s)?;
        Ok(proto::Empty {})
    })
}

fn row_agent(conn: &Connection, request_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT agent_id FROM owner_input_requests WHERE id = ?1",
        [request_id],
        |r| r.get(0),
    )?)
}

/// The driver closes its own pending request, by id or by sequence. The
/// sequence barrier rises either way, so a publish delayed past a turn's
/// cancellation can never create a stale request. Generation and usage are
/// untouched.
pub fn close_owner_input(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::CloseOwnerInputRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        validate_generation(ctx, &me, &req.generation)?;
        if let Some(seq) = req.request_seq
            && seq < 1
        {
            bail!("close sequence must be positive");
        }
        let mut barrier = None;
        if let Some(id) = req.request_id {
            let Some(row) = db::owner_input(ctx.conn, id)? else {
                bail!("no such input request");
            };
            if row_agent(ctx.conn, id)? != me.id || row.generation != req.generation {
                bail!("not your input request");
            }
            if let Some(seq) = req.request_seq
                && seq != row.seq
            {
                bail!("request id and sequence disagree");
            }
            // Retrying a successful close after a lost ACK is a no-op; any
            // other terminal state is an error.
            if row.status == "pending" {
                db::finish_owner_input(ctx.conn, row.id, None, "cancelled", &ctx.now_s)?;
            } else if row.status != "cancelled" {
                bail!("input request is not pending");
            }
            barrier = Some(row.seq);
        }
        if let Some(seq) = req.request_seq {
            if req.request_id.is_none()
                && let Some(pending) = db::pending_owner_input(ctx.conn, me.id)?
                && pending.seq == seq
            {
                db::finish_owner_input(ctx.conn, pending.id, None, "cancelled", &ctx.now_s)?;
            }
            barrier = Some(barrier.map_or(seq, |b| b.max(seq)));
        }
        let Some(barrier) = barrier else {
            bail!("close needs a request id or sequence");
        };
        db::close_owner_input_seq(ctx.conn, me.id, barrier)?;
        Ok(proto::Empty {})
    })
}

// ---- isolated side questions (side-questions.v1) ----------------------------

fn side_status_proto(status: &str) -> proto::SideStatus {
    match status {
        "completed" => proto::SideStatus::SideCompleted,
        "failed" => proto::SideStatus::SideFailed,
        "cancelled" => proto::SideStatus::SideCancelled,
        "interrupted" => proto::SideStatus::SideInterrupted,
        "ended" => proto::SideStatus::SideEnded,
        _ => proto::SideStatus::SideOpen,
    }
}

/// A terminal side conversation idle this long cannot be continued; the
/// runner keeps its child a minute longer.
const SIDE_IDLE_LIMIT: chrono::TimeDelta = chrono::TimeDelta::minutes(30);

/// Statuses a follow-up may continue from.
fn side_continuable_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Whether `row`'s conversation cannot be continued, derived at read time:
/// stored ended, INTERRUPTED or ENDED, claimed by a generation other than
/// the current one, or terminal and either never claimed or idle past the
/// limit. A new open row that no runner has claimed yet is not ended.
fn side_ended(row: &db::SideRequestRow, state: Option<&db::SideState>, now: DateTime<Utc>) -> bool {
    if row.ended || matches!(row.status.as_str(), "interrupted" | "ended") {
        return true;
    }
    let current = state.map(|s| s.generation.as_str());
    match &row.generation {
        Some(g) if Some(g.as_str()) != current => return true,
        Some(_) => {}
        None => return row.status != "open",
    }
    row.status != "open"
        && DateTime::parse_from_rfc3339(&row.updated_at)
            .ok()
            .is_none_or(|t| now - t.with_timezone(&Utc) >= SIDE_IDLE_LIMIT)
}

fn side_request_proto(
    row: db::SideRequestRow,
    state: Option<&db::SideState>,
    now: DateTime<Utc>,
) -> Result<proto::SideRequest> {
    Ok(proto::SideRequest {
        ended: side_ended(&row, state, now),
        conversation: Some(row.conversation),
        follows: row.follows,
        id: row.id,
        question: row.question,
        status: side_status_proto(&row.status).into(),
        answer: row.answer,
        updated_at: row.updated_at,
    })
}

/// The owner asks the agent an isolated side question; one open at a time.
/// A follow-up continues the conversation whose latest question it names,
/// checked and inserted in this one write transaction.
pub fn ask_side(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AskSideRequest,
) -> Result<proto::AskSideResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let Some(agent) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
            bail!("no live agent named {:?}", req.agent);
        };
        if db::agent_owner_id(ctx.conn, agent.id)? != Some(me.id) {
            bail!("{:?} is not your agent", req.agent);
        }
        if req.question.is_empty() || req.question.len() > 4 * 1024 {
            bail!("question must be 1 byte to 4 KiB");
        }
        let state = match db::side_state(ctx.conn, agent.id)? {
            Some(state) if state.supported == Some(true) => state,
            _ => bail!("{}'s runner does not answer side questions", agent.name),
        };
        if db::open_side_request(ctx.conn, agent.id)?.is_some() {
            bail!("a side question is already open");
        }
        let followed = match req.follows {
            None => None,
            Some(follows) => {
                if state.followups != Some(true) {
                    bail!("follow-ups need an updated runner");
                }
                let latest = db::latest_side_request(ctx.conn, agent.id)?;
                match latest {
                    Some(row)
                        if row.id == follows
                            && side_continuable_status(&row.status)
                            && !side_ended(&row, Some(&state), ctx.now) =>
                    {
                        Some(row)
                    }
                    _ => bail!("side conversation ended"),
                }
            }
        };
        let id = db::insert_side_request(
            ctx.conn,
            agent.id,
            &req.question,
            followed.as_ref(),
            &ctx.now_s,
        )?;
        Ok(proto::AskSideResponse { id })
    })
}

/// The owner cancels a side question by exact id; idempotent once cancelled.
/// With `end`, the owner ends the conversation whose latest question is
/// `id`, cancelling it too if open; idempotent.
pub fn cancel_side(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::CancelSideRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let Some(row) = db::side_request(ctx.conn, req.id)? else {
            bail!("no such side question");
        };
        if db::agent_owner_id(ctx.conn, row.agent_id)? != Some(me.id) {
            bail!("not your agent's question");
        }
        if req.end {
            if db::latest_side_request(ctx.conn, row.agent_id)?.map(|r| r.id) != Some(row.id) {
                bail!("not the agent's latest side question");
            }
            if row.status == "open" {
                db::cancel_side_request(ctx.conn, row.id, &ctx.now_s)?;
            }
            if !row.ended {
                db::end_side_request(ctx.conn, row.id, &ctx.now_s)?;
            }
            return Ok(proto::Empty {});
        }
        // Idempotent after a lost reply: already cancelled is a no-op.
        if row.status == "cancelled" {
            return Ok(proto::Empty {});
        }
        if row.status != "open" {
            bail!("side question is not open");
        }
        db::cancel_side_request(ctx.conn, req.id, &ctx.now_s)?;
        Ok(proto::Empty {})
    })
}

/// The runner claims the agent's one open side question. Idempotent per
/// generation: a lost reply's retry returns the same claim.
pub fn claim_side(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ClaimSideRequest,
) -> Result<proto::ClaimSideResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        validate_generation(ctx, &me, &req.generation)?;
        let Some(row) = db::open_side_request(ctx.conn, me.id)? else {
            return Ok(proto::ClaimSideResponse { request: None });
        };
        let claimed = match &row.generation {
            None => db::claim_side_request(ctx.conn, me.id, &req.generation, &ctx.now_s)?,
            Some(g) if *g == req.generation => true,
            Some(_) => bail!("side question already claimed"),
        };
        let request = if claimed {
            let row = db::open_side_request(ctx.conn, me.id)?.expect("just claimed");
            let state = db::side_state(ctx.conn, me.id)?;
            Some(side_request_proto(row, state.as_ref(), ctx.now)?)
        } else {
            // A concurrent claim won the race; treat it as interrupted.
            None
        };
        Ok(proto::ClaimSideResponse { request })
    })
}

/// The claiming runner reports progressive text or a terminal result.
pub fn report_side(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReportSideRequest,
) -> Result<proto::Empty> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        validate_generation(ctx, &me, &req.generation)?;
        if req.answer.as_deref().is_some_and(|a| a.len() > 64 * 1024) {
            bail!("answer exceeds 64 KiB");
        }
        let status = match req.status() {
            proto::SideStatus::SideOpen => "open",
            proto::SideStatus::SideCompleted => "completed",
            proto::SideStatus::SideFailed => "failed",
            proto::SideStatus::SideEnded => return report_side_ended(ctx, &me, &req),
            _ => bail!("a side report must be OPEN, COMPLETED, FAILED or ENDED"),
        };
        let Some(row) = db::side_request(ctx.conn, req.id)? else {
            bail!("no such side question");
        };
        if row.agent_id != me.id || row.generation.as_deref() != Some(req.generation.as_str()) {
            bail!("not your claimed side question");
        }
        if row.status != "open" {
            bail!("side question is closed");
        }
        db::report_side_request(ctx.conn, req.id, req.answer.as_deref(), status, &ctx.now_s)?;
        Ok(proto::Empty {})
    })
}

/// ENDED from the runner, never after a model call: an open follow-up whose
/// conversation it no longer holds ends with that status; the latest
/// terminal question it answered keeps its status and is flagged ended when
/// its child is lost. A repeat, or a question a newer one has replaced
/// (pruned or not), is a no-op so the runner never retries in a loop.
fn report_side_ended(
    ctx: &Ctx,
    me: &db::Principal,
    req: &proto::ReportSideRequest,
) -> Result<proto::Empty> {
    let latest = db::latest_side_request(ctx.conn, me.id)?;
    let Some(row) = db::side_request(ctx.conn, req.id)? else {
        if latest.is_some_and(|l| l.id > req.id) {
            return Ok(proto::Empty {});
        }
        bail!("no such side question");
    };
    if row.agent_id != me.id || row.generation.as_deref() != Some(req.generation.as_str()) {
        bail!("not your claimed side question");
    }
    if row.status == "open" {
        if row.follows.is_none() {
            bail!("only a follow-up can be reported ENDED while open");
        }
        db::report_side_request(ctx.conn, row.id, req.answer.as_deref(), "ended", &ctx.now_s)?;
        return Ok(proto::Empty {});
    }
    let replaced = latest.is_some_and(|l| l.id != row.id);
    if !replaced && !row.ended && row.status != "ended" {
        db::end_side_request(ctx.conn, row.id, &ctx.now_s)?;
    }
    Ok(proto::Empty {})
}

/// One agent's side state: the owner, or the driver with its generation.
pub fn side_report(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SideReportRequest,
) -> Result<proto::SideReportResponse> {
    with_ctx(conn, caller, |ctx| {
        let agent = match &req.generation {
            Some(generation) => {
                let me = ctx.agent()?;
                validate_generation(ctx, &me, generation)?;
                me
            }
            None => {
                let human = ctx.human()?;
                let Some(target) = db::live_agent_by_name(ctx.conn, &req.agent)? else {
                    bail!("no live agent named {:?}", req.agent);
                };
                if db::agent_owner_id(ctx.conn, target.id)? != Some(human.id) {
                    bail!("{:?} is not your agent", req.agent);
                }
                target
            }
        };
        let state = db::side_state(ctx.conn, agent.id)?;
        let pending = db::latest_owner_input(ctx.conn, agent.id)?
            .map(owner_input_proto)
            .transpose()?;
        let side = db::latest_side_request(ctx.conn, agent.id)?
            .map(|row| side_request_proto(row, state.as_ref(), ctx.now))
            .transpose()?;
        let Some(state) = state else {
            return Ok(proto::SideReportResponse {
                side_followups: None,
                agent: agent.name,
                pending_input: pending,
                usage: None,
                revision: 0,
                reported_at: String::new(),
                side,
                side_supported: None,
                side_reason: None,
            });
        };
        Ok(proto::SideReportResponse {
            side_followups: state.followups,
            agent: agent.name,
            pending_input: pending,
            usage: usage_proto(state.usage)?,
            revision: state.revision,
            reported_at: state.reported_at,
            side,
            side_supported: state.supported,
            side_reason: state.reason,
        })
    })
}

/// Delivery keys are bounded ASCII identifiers, scoped to the sender's
/// registration.
fn validate_delivery_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
    if !ok {
        bail!("delivery key must be 1 to 128 bytes of [A-Za-z0-9._:-]");
    }
    Ok(())
}

/// An agent's automatic reply to its owning human (auto-reply.v1). The
/// destination is the registered owner, never a request field. The delivery
/// key makes retries safe: a replayed send returns the original message
/// without delivering again.
pub fn auto_reply(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AutoReplyRequest,
) -> Result<proto::AutoReplyResponse> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.actor()?;
        if me.kind != db::Kind::Agent {
            bail!("only agents send automatic owner replies");
        }
        validate_delivery_key(&req.key)?;
        // Unlike an ordinary send, whitespace-only bodies are kept: runners
        // stream parts whose bytes must survive byte-for-byte.
        if req.body.is_empty() {
            bail!("message body is empty");
        }
        if req.body.len() > MAX_BODY_BYTES {
            bail!("message body exceeds {} KiB", MAX_BODY_BYTES / 1024);
        }
        let owner = db::agent_owner_id(ctx.conn, me.id)?
            .ok_or_else(|| anyhow!("{} has no registered owner", me.name))?;
        if let Some((message_id, body, recipient)) = db::find_auto_reply(ctx.conn, me.id, &req.key)?
        {
            if body != req.body {
                bail!("delivery key reused with a different body");
            }
            if recipient != owner {
                bail!("delivery key's owner no longer matches the registered owner");
            }
            // A retry after a lost reply: the same mail, sent exactly once.
            return Ok(proto::AutoReplyResponse {
                id: message_id,
                replayed: true,
            });
        }
        let msg = db::insert_message(ctx.conn, &me, None, &req.body, &ctx.now_s)?;
        db::set_message_auto_reply(ctx.conn, msg)?;
        db::insert_delivery(ctx.conn, msg, db::Kind::Human, owner)?;
        db::record_human_conversation(ctx.conn, me.id, owner, &ctx.now_s)?;
        db::insert_auto_reply(ctx.conn, me.id, &req.key, msg)?;
        tracing::info!(from = me.name, bytes = req.body.len(), "sent auto reply");
        Ok(proto::AutoReplyResponse {
            id: msg,
            replayed: false,
        })
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
        let counts = db::blockers::agent_counts(ctx.conn)?;
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
                id: Some(a.id),
                blocker_count: Some(counts.get(&a.id).copied().unwrap_or(0)),
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

fn message_groups(conn: &Connection, name: Option<&str>) -> Result<Option<Vec<i64>>> {
    name.map(group_name)
        .map(|name| {
            let ids = db::group_ids_named(conn, name)?;
            if ids.is_empty() {
                bail!("no such group @{name}");
            }
            Ok(ids)
        })
        .transpose()
}

fn message_principals(
    conn: &Connection,
    name: Option<&str>,
) -> Result<Option<Vec<(db::Kind, i64)>>> {
    name.map(|name| {
        let ids = db::principals_named(conn, name)?;
        if ids.is_empty() {
            bail!("no agent or human named {name:?}");
        }
        Ok(ids)
    })
    .transpose()
}

/// Search all ordinary messages, regardless of the caller's inbox or groups.
/// Actor authentication/activity bookkeeping is normal; message/delivery state
/// is never changed.
pub fn search(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::SearchRequest,
) -> Result<proto::SearchResponse> {
    // Validate before taking a transaction or scanning any messages.
    let limit = req.limit.unwrap_or(crate::search::DEFAULT_LIMIT);
    if !(1..=crate::search::MAX_LIMIT).contains(&limit) {
        bail!(
            "search limit must be 1 to {}, not {limit}",
            crate::search::MAX_LIMIT
        );
    }
    if req.before_id.is_some_and(|id| id <= 0) {
        bail!("before_id must be positive");
    }
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
    with_ctx(conn, caller, |ctx| {
        ctx.actor()?;
        let matcher = crate::search::Matcher::new(&req.query, req.case_sensitive)?;
        let mut filter = db::FeedFilter {
            group_ids: message_groups(ctx.conn, req.group.as_deref())?,
            senders: message_principals(ctx.conn, req.from.as_deref())?,
            with: message_principals(ctx.conn, req.with.as_deref())?,
            since: since.as_deref(),
            until: until.as_deref(),
            before_id: Some(req.before_id.unwrap_or(i64::MAX)),
            ..Default::default()
        };
        let (hits, has_more) =
            crate::search::messages(ctx.conn, &matcher, limit as usize, &mut filter)?;
        let next_before_id = has_more.then(|| hits.last().unwrap().message.id);
        let hits = hits
            .into_iter()
            .map(|h| proto::SearchHit {
                truncated: h.message.body.len() != h.original_body_bytes,
                body_offset_bytes: h.body_offset_bytes as u32,
                original_body_bytes: h.original_body_bytes as u32,
                match_offset_bytes: h.matched.start as u32,
                match_length_bytes: h.matched.len() as u32,
                message: Some(message(h.message, None)),
            })
            .collect();
        Ok(proto::SearchResponse {
            hits,
            has_more,
            next_before_id,
        })
    })
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
        if req.sync {
            return sync_page(ctx.conn, me.id, &req);
        }
        if req.include_group_deliveries && req.with.is_none() {
            bail!("include_group_deliveries requires a with filter");
        }
        let group_ids = message_groups(ctx.conn, req.group.as_deref())?;
        let senders = message_principals(ctx.conn, req.from.as_deref())?;
        let with = message_principals(ctx.conn, req.with.as_deref())?;
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
            include_group_deliveries: req.include_group_deliveries,
            since: since.as_deref(),
            until: until.as_deref(),
            scope: req.mine.then_some(me.id),
            after_id: req.after_id,
            before_id: None,
        };
        let messages = db::feed_messages(ctx.conn, req.backlog, &filter)?
            .into_iter()
            .map(|m| message(m, None))
            .collect();
        Ok(proto::FeedSnapshot {
            messages,
            ..Default::default()
        })
    })
}

/// Most encoded bytes in one sync snapshot: tonic's default decoder limit, so
/// any client can read a page, and far below this client's own.
pub const MAX_SYNC_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// One page of the caller's personal history after `after_id` (feed-sync.v1):
/// at most `backlog` messages and `MAX_SYNC_PAGE_BYTES`, each with the
/// caller's agents whose conversation holds it, and whether nothing further
/// matched when it was read. A message is never cut short; one that would
/// cross the byte budget waits for the next page.
fn sync_page(
    conn: &Connection,
    human: i64,
    req: &proto::FeedRequest,
) -> Result<proto::FeedSnapshot> {
    let filtered = req.group.is_some()
        || req.from.is_some()
        || req.from_human.is_some()
        || req.with.is_some()
        || req.since.is_some()
        || req.until.is_some()
        || req.include_group_deliveries;
    if !req.mine || filtered {
        bail!("feed sync is for the caller's whole personal history: mine, with no other filter");
    }
    let Some(after) = req.after_id.filter(|after| *after >= 0) else {
        bail!("feed sync needs a cursor: after_id of 0 or more");
    };
    let filter = db::FeedFilter {
        group_ids: None,
        senders: None,
        from_human: None,
        with: None,
        include_group_deliveries: false,
        since: None,
        until: None,
        scope: Some(human),
        after_id: Some(after),
        before_id: None,
    };
    // One row past the page, in the same read, says whether more follow.
    let rows = db::feed_messages(conn, req.backlog + 1, &filter)?;
    let ids: Vec<i64> = rows.iter().map(|m| m.id).collect();
    let mut scopes = db::owned_agent_scopes(conn, human, &ids)?;
    let mut page = proto::FeedSnapshot::default();
    let mut bytes = 0;
    let mut caught_up = true;
    for row in rows {
        let scope = proto::FeedAgentScope {
            message_id: row.id,
            agents: scopes
                .remove(&row.id)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, name)| proto::FeedAgent { id, name })
                .collect(),
        };
        let message = message(row, None);
        // Each field's tag and length prefix, generously.
        let size =
            prost::Message::encoded_len(&message) + prost::Message::encoded_len(&scope) + 2 * 16;
        let full = page.messages.len() == req.backlog as usize;
        if full || (!page.messages.is_empty() && bytes + size > MAX_SYNC_PAGE_BYTES) {
            caught_up = false;
            break;
        }
        bytes += size;
        page.messages.push(message);
        page.agent_scopes.push(scope);
    }
    page.caught_up = Some(caught_up);
    Ok(page)
}

/// After sending a cursor-mode snapshot, move the cursor past it. Returns
/// whether more matching messages may be waiting, so the feed should fetch
/// again before waiting for the next change: a full batch, or in sync mode a
/// page that is not caught up.
pub fn advance_feed_cursor(req: &mut proto::FeedRequest, sent: &proto::FeedSnapshot) -> bool {
    let Some(after) = &mut req.after_id else {
        return false;
    };
    if let Some(last) = sent.messages.last() {
        *after = last.id;
    }
    if req.sync {
        return sent.caught_up == Some(false);
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
        bail!(
            "{} choices; the limit is {MAX_EFFORT_CHOICES}",
            state.choices.len()
        );
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
        let body =
            db::effort_control_body(ctx.conn, agent, pending.request_id)?.expect("checked above");
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
        bail!(
            "request {} is both pending and answered",
            pending.request_id
        );
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
            result: proto::effort_outcome::Result::from_str_name(o.get("result")?.as_str()?)?
                as i32,
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
        pending: v
            .get("pending")
            .and_then(|p| if p.is_null() { None } else { request(p) }),
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
        let counts = db::blockers::agent_counts(ctx.conn)?;
        let agents = db::agent_summaries(ctx.conn, me.id)?
            .into_iter()
            .map(|s| proto::AgentSummary {
                agent_id: Some(s.agent_id),
                blocker_count: Some(counts.get(&s.agent_id).copied().unwrap_or(0)),
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
        Ok(proto::AgentSummarySnapshot {
            agents,
            notes_revision: Some(db::notes::max_revision(ctx.conn, me.id)? as u64),
        })
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

    fn search_req(query: &str) -> proto::SearchRequest {
        proto::SearchRequest {
            query: query.into(),
            ..Default::default()
        }
    }

    fn search_ids(r: &proto::SearchResponse) -> Vec<i64> {
        r.hits
            .iter()
            .map(|h| h.message.as_ref().unwrap().id)
            .collect()
    }

    fn search_insert(
        conn: &Connection,
        from: &db::Principal,
        to: &db::Principal,
        body: &str,
        time: &str,
    ) -> i64 {
        let id = db::insert_message(conn, from, None, body, time).unwrap();
        db::insert_delivery(conn, id, to.kind, to.id).unwrap();
        id
    }

    #[test]
    fn search_is_literal_unicode_board_wide_and_does_not_read_mail() {
        let (mut conn, will, [mateo, _, lu]) = board();
        let one = send(
            &mut conn,
            &will,
            proto::SendRequest {
                to: "Lu".into(),
                body: "**ÉQUIPE** a.*%_[]\nnext line".into(),
                control: None,
            },
        )
        .unwrap()
        .id
        .unwrap();
        // A different owner's private DM; Mateo shares neither its owner nor a group.
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        let ada = Caller {
            human: Some("ada".into()),
            ..Default::default()
        };
        let eve = Caller {
            token: Some(
                register(
                    &mut conn,
                    &ada,
                    proto::RegisterRequest {
                        name: Some("Eve".into()),
                        ..Default::default()
                    },
                )
                .unwrap()
                .token,
            ),
            ..Default::default()
        };
        let private = send(
            &mut conn,
            &ada,
            proto::SendRequest {
                to: "Eve".into(),
                body: "équipe private".into(),
                control: None,
            },
        )
        .unwrap()
        .id
        .unwrap();
        read(&mut conn, &eve, proto::ReadRequest::default()).unwrap();
        let retired = send(
            &mut conn,
            &eve,
            proto::SendRequest {
                to: "ada".into(),
                body: "équipe reply".into(),
                control: None,
            },
        )
        .unwrap()
        .id
        .unwrap();
        retire_agent(
            &mut conn,
            &ada,
            proto::RetireRequest {
                agent: "Eve".into(),
            },
        )
        .unwrap();
        send(
            &mut conn,
            &will,
            proto::SendRequest {
                to: "@crew".into(),
                body: "équipe group".into(),
                control: None,
            },
        )
        .unwrap();
        let group: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE body = ?",
                ["équipe group"],
                |r| r.get(0),
            )
            .unwrap();
        let control = send(
            &mut conn,
            &will,
            proto::SendRequest {
                to: "Lu".into(),
                body: "équipe control".into(),
                control: None,
            },
        )
        .unwrap()
        .id
        .unwrap();
        db::set_message_control(&conn, control, "cancel").unwrap();
        let before: Vec<_> = conn.prepare("SELECT message_id, recipient_id, archived_at FROM deliveries ORDER BY message_id, recipient_id")
            .unwrap().query_map([], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,i64>(1)?, row.get::<_,Option<String>>(2)?)))
            .unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        let got = search(&mut conn, &mateo, search_req("équipe")).unwrap();
        assert_eq!(search_ids(&got), [group, retired, private, one]);
        assert!(!got.has_more && got.next_before_id.is_none());
        assert_eq!(
            got.hits[2].message.as_ref().unwrap().to_name.as_deref(),
            Some("Eve")
        );
        assert_eq!(got.hits[2].message.as_ref().unwrap().from_name, "ada");
        let after: Vec<_> = conn.prepare("SELECT message_id, recipient_id, archived_at FROM deliveries ORDER BY message_id, recipient_id")
            .unwrap().query_map([], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,i64>(1)?, row.get::<_,Option<String>>(2)?)))
            .unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        assert_eq!(before, after, "search never marks anybody's mail read");
        let mut exact = search_req("équipe");
        exact.case_sensitive = true;
        assert_eq!(
            search_ids(&search(&mut conn, &lu, exact).unwrap()),
            [group, retired, private]
        );
        assert_eq!(
            search_ids(&search(&mut conn, &mateo, search_req("a.*%_[]\nnext")).unwrap()),
            [one]
        );
        assert!(
            search(&mut conn, &mateo, search_req("equipe"))
                .unwrap()
                .hits
                .is_empty()
        );
        let mut filtered = search_req("équipe");
        filtered.from = Some("eVe".into());
        assert_eq!(
            search_ids(&search(&mut conn, &mateo, filtered).unwrap()),
            [retired]
        );
        assert!(search(&mut conn, &Caller::default(), search_req("x")).is_err());
        assert!(
            search(&mut conn, &eve, search_req("x")).is_err(),
            "retired tokens fail"
        );
    }

    #[test]
    fn search_filters_and_cursor_apply_before_the_count_limit() {
        let (mut conn, will, [mateo, _, _]) = board();
        let owner = db::find_human(&conn, "will").unwrap().unwrap();
        let agent = db::live_agent_by_name(&conn, "Mateo").unwrap().unwrap();
        // More than a scan batch of misses, and more than the maximum result count.
        let mut wanted = Vec::new();
        for i in 0..260 {
            let body = if i % 2 == 0 { "needle" } else { "miss" };
            // Deliberately nonmonotonic times: results must still order by id.
            let time = if i % 4 == 0 {
                "2020-01-01T00:00:00.000000Z"
            } else {
                "2020-01-02T00:00:00.000000Z"
            };
            let id = search_insert(&conn, &owner, &agent, body, time);
            if i % 2 == 0 {
                wanted.push(id);
            }
        }
        wanted.reverse();
        assert_eq!(
            search_ids(&search(&mut conn, &mateo, search_req("needle")).unwrap()),
            wanted[..20]
        );
        let mut req = search_req("needle");
        req.limit = Some(100);
        let page1 = search(&mut conn, &mateo, req.clone()).unwrap();
        assert_eq!(search_ids(&page1), wanted[..100]);
        assert_eq!(page1.next_before_id, Some(wanted[99]));
        search_insert(
            &conn,
            &owner,
            &agent,
            "needle newest",
            "2020-01-03T00:00:00.000000Z",
        );
        req.before_id = page1.next_before_id;
        let page2 = search(&mut conn, &mateo, req).unwrap();
        assert_eq!(search_ids(&page2), wanted[100..]);
        assert!(!page2.has_more && page2.next_before_id.is_none());
        let mut req = search_req("needle");
        req.from = Some("WILL".into());
        req.with = Some("mateo".into());
        req.since = Some("2020-01-02T00:00:00Z".into());
        req.until = Some("2020-01-03T00:00:00Z".into());
        req.limit = Some(1);
        let got = search(&mut conn, &mateo, req).unwrap();
        assert_eq!(
            search_ids(&got),
            [wanted[0]],
            "all filters intersect before limit; id order wins"
        );
        send(
            &mut conn,
            &will,
            proto::SendRequest {
                to: "@crew".into(),
                body: "needle group".into(),
                control: None,
            },
        )
        .unwrap();
        let group: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE body = ?",
                ["needle group"],
                |r| r.get(0),
            )
            .unwrap();
        let mut req = search_req("needle");
        req.group = Some("@crew".into());
        assert_eq!(
            search_ids(&search(&mut conn, &mateo, req.clone()).unwrap()),
            [group]
        );
        req.with = Some("Mateo".into());
        assert!(
            search(&mut conn, &mateo, req).unwrap().hits.is_empty(),
            "with does not mean group delivery"
        );
        for req in [
            proto::SearchRequest {
                from: Some("missing".into()),
                ..search_req("x")
            },
            proto::SearchRequest {
                with: Some("missing".into()),
                ..search_req("x")
            },
            proto::SearchRequest {
                group: Some("missing".into()),
                ..search_req("x")
            },
            proto::SearchRequest {
                limit: Some(0),
                ..search_req("x")
            },
            proto::SearchRequest {
                limit: Some(101),
                ..search_req("x")
            },
            proto::SearchRequest {
                before_id: Some(0),
                ..search_req("x")
            },
            proto::SearchRequest {
                before_id: Some(-1),
                ..search_req("x")
            },
            proto::SearchRequest {
                since: Some("bad".into()),
                ..search_req("x")
            },
            proto::SearchRequest {
                since: Some("2020-01-02T00:00:00Z".into()),
                until: Some("2020-01-02T00:00:00Z".into()),
                ..search_req("x")
            },
            search_req(""),
            search_req(&"x".repeat(4097)),
        ] {
            assert!(search(&mut conn, &mateo, req).is_err());
        }
    }

    #[test]
    fn search_text_budget_preserves_pagination_and_marks_original_excerpts() {
        let (mut conn, _, [mateo, _, _]) = board();
        conn.execute("DELETE FROM deliveries", []).unwrap();
        conn.execute("DELETE FROM messages", []).unwrap();
        let owner = db::find_human(&conn, "will").unwrap().unwrap();
        let agent = db::live_agent_by_name(&conn, "Mateo").unwrap().unwrap();
        let one = search_insert(
            &conn,
            &owner,
            &agent,
            &"a".repeat(17000),
            "2020-01-01T00:00:00Z",
        );
        let two = search_insert(
            &conn,
            &owner,
            &agent,
            &"a".repeat(16000),
            "2020-01-01T00:00:00Z",
        );
        let got = search(&mut conn, &mateo, search_req("a")).unwrap();
        assert_eq!(search_ids(&got), [two]);
        assert_eq!(got.next_before_id, Some(two));
        let got = search(
            &mut conn,
            &mateo,
            proto::SearchRequest {
                before_id: got.next_before_id,
                ..search_req("a")
            },
        )
        .unwrap();
        assert_eq!(search_ids(&got), [one]);
        assert!(!got.has_more);
        for position in [0, 30000, 60000] {
            let body = format!(
                "{}ÉQUIPE{}",
                "界".repeat(position / 3),
                "界".repeat((64000 - position) / 3)
            );
            let id = search_insert(&conn, &owner, &agent, &body, "2020-01-01T00:00:00Z");
            let got = search(&mut conn, &mateo, search_req("équipe")).unwrap();
            let h = &got.hits[0];
            let m = h.message.as_ref().unwrap();
            assert_eq!(m.id, id);
            assert!(h.truncated);
            assert!(m.body.len() <= crate::search::TEXT_BUDGET);
            assert_eq!(h.original_body_bytes as usize, body.len());
            assert_eq!(
                &body[h.body_offset_bytes as usize..h.body_offset_bytes as usize + m.body.len()],
                m.body
            );
            assert!(m.body.contains("ÉQUIPE"));
            assert_eq!(
                &body[h.match_offset_bytes as usize
                    ..(h.match_offset_bytes + h.match_length_bytes) as usize],
                "ÉQUIPE"
            );
            assert!(
                got.hits
                    .iter()
                    .map(|h| h.message.as_ref().unwrap().body.len())
                    .sum::<usize>()
                    <= crate::search::TEXT_BUDGET
            );
        }
        let exact = "z".repeat(crate::search::TEXT_BUDGET);
        let id = search_insert(&conn, &owner, &agent, &exact, "2020-01-01T00:00:00Z");
        let got = search(&mut conn, &mateo, search_req("z")).unwrap();
        assert_eq!(search_ids(&got), [id]);
        assert!(!got.hits[0].truncated);
        assert_eq!(got.hits[0].message.as_ref().unwrap().body, exact);
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
                      answered: Option<proto::EffortOutcome>| {
            proto::ReportEffortRequest {
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
            }
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
        assert!(
            format!("{err:#}").contains("not a set-effort control"),
            "{err:#}"
        );
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
        assert!(
            format!("{err:#}").contains("not a set-effort control"),
            "{err:#}"
        );
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
        assert_eq!(answered.result(), proto::effort_outcome::Result::Applied);
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
        assert!(
            format!("{err:#}").contains("both pending and answered"),
            "{err:#}"
        );
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
    fn auto_replies_reach_the_owner_once_and_replay_safely() {
        let (mut conn, will, [mateo, zed, _lu]) = board();
        let reply = |conn: &mut Connection, key: &str, body: &str| {
            auto_reply(
                conn,
                &mateo,
                proto::AutoReplyRequest {
                    key: key.into(),
                    body: body.into(),
                },
            )
        };
        // Humans cannot send automatic replies; the request has no `to`.
        let human = auto_reply(
            &mut conn,
            &will,
            proto::AutoReplyRequest {
                key: "k".into(),
                body: "hi".into(),
            },
        );
        assert!(human.unwrap_err().to_string().contains("only agents"));
        // An agent without a registered owner has nowhere to reply.
        conn.execute("DELETE FROM agent_owners WHERE agent_id = 3", [])
            .unwrap();
        let unowned = auto_reply(
            &mut conn,
            &_lu,
            proto::AutoReplyRequest {
                key: "k".into(),
                body: "hi".into(),
            },
        );
        assert!(
            unowned
                .unwrap_err()
                .to_string()
                .contains("no registered owner")
        );
        // Key and body bounds.
        for key in ["", "no spaces", "snowman-\u{2603}", &"k".repeat(129)] {
            let err = reply(&mut conn, key, "hi").unwrap_err().to_string();
            assert!(err.contains("delivery key"), "{key:?}: {err}");
        }
        let err = reply(&mut conn, "k", "").unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
        let err = reply(&mut conn, "k", &"x".repeat(64 * 1024 + 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "{err}");
        // Whitespace-only bodies are valid streamed parts; every byte is
        // preserved, exactly as sent. Zero-byte bodies stay invalid.
        let spaces = reply(&mut conn, "part-1", "  \n ").unwrap();
        let mail = inbox(&mut conn, &will);
        assert!(mail.iter().any(|m| m.id == spaces.id && m.body == "  \n "));

        // The destination is the registered owner, resolved server-side.
        let first = reply(&mut conn, "turn-1", "done").unwrap();
        assert!(!first.replayed);
        let mail = inbox(&mut conn, &will);
        let auto: Vec<_> = mail.iter().filter(|m| m.auto_reply == Some(true)).collect();
        assert_eq!(auto.len(), 1);
        assert_eq!(
            (
                auto[0].id,
                auto[0].from_name.as_str(),
                auto[0].body.as_str()
            ),
            (first.id, "Mateo", "done")
        );
        assert_eq!(auto[0].to_name.as_deref(), Some("will"));
        // Manual mail is not marked.
        let manual = send(
            &mut conn,
            &zed,
            proto::SendRequest {
                to: "will".into(),
                body: "by hand".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            inbox(&mut conn, &will)
                .iter()
                .any(|m| m.id == manual.id.unwrap() && m.auto_reply.is_none())
        );
        // The feed carries the provenance too.
        let feed = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            feed.messages
                .iter()
                .any(|m| m.id == first.id && m.auto_reply == Some(true))
        );

        // The normal send effects happened exactly once. Make the pair's
        // timestamp observably old so a replay that wrongly touches recency
        // cannot hide behind back-to-back timestamps.
        let recency = |conn: &mut Connection| -> String {
            conn.query_row(
                "SELECT last_at FROM agent_human_conversations WHERE agent_id = 1 AND human_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        conn.execute(
            "UPDATE agent_human_conversations SET last_at = '2020-01-01T00:00:00.000000Z'
             WHERE agent_id = 1 AND human_id = 1",
            [],
        )
        .unwrap();
        let old_recency = recency(&mut conn);

        // A retry after a lost reply returns the same mail and changes
        // nothing: no second delivery, no recreated unread (the owner already
        // read it above), no recency update.
        let again = reply(&mut conn, "turn-1", "done").unwrap();
        assert!(again.replayed && again.id == first.id);
        let deliveries: i64 = conn
            .query_row(
                "SELECT count(*) FROM deliveries WHERE message_id = ?1",
                [first.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deliveries, 1);
        assert!(
            !inbox(&mut conn, &will)
                .iter()
                .any(|m| m.auto_reply == Some(true) && m.body == "done"),
            "a replay must not recreate unread mail"
        );
        assert_eq!(recency(&mut conn), old_recency);

        // The same key with a different body is rejected; nothing is sent.
        let err = reply(&mut conn, "turn-1", "changed")
            .unwrap_err()
            .to_string();
        assert!(err.contains("different body"), "{err}");

        // A different key is a new send.
        let second = reply(&mut conn, "turn-2", "also done").unwrap();
        assert!(!second.replayed && second.id != first.id);

        // whoami exposes the owner to agents, never to humans.
        let me = whoami(&mut conn, &mateo).unwrap();
        assert_eq!(me.owner.as_deref(), Some("will"));
        assert!(whoami(&mut conn, &will).unwrap().owner.is_none());

        // Registration isolation: a retired name's keys do not follow its
        // re-registration.
        retire_agent(
            &mut conn,
            &will,
            proto::RetireRequest {
                agent: "Mateo".into(),
            },
        )
        .unwrap();
        let token = register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let mateo2 = Caller {
            token: Some(token),
            ..Default::default()
        };
        let fresh = auto_reply(
            &mut conn,
            &mateo2,
            proto::AutoReplyRequest {
                key: "turn-1".into(),
                body: "done".into(),
            },
        )
        .unwrap();
        assert!(!fresh.replayed && fresh.id != first.id);

        // Owner reassignment between retries: a replay whose stored recipient
        // is no longer the owner is rejected, with no new mail.
        let third = auto_reply(
            &mut conn,
            &mateo2,
            proto::AutoReplyRequest {
                key: "turn-2".into(),
                body: "same".into(),
            },
        )
        .unwrap();
        assert!(!third.replayed);
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        conn.execute(
            "UPDATE agent_owners SET human_id = (SELECT id FROM humans WHERE name = 'ada')
             WHERE agent_id = (SELECT id FROM agents WHERE name = 'Mateo' AND retired_at IS NULL)",
            [],
        )
        .unwrap();
        let err = auto_reply(
            &mut conn,
            &mateo2,
            proto::AutoReplyRequest {
                key: "turn-2".into(),
                body: "same".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("owner no longer matches"), "{err}");
        // Nothing new reached either human from that key.
        let will_mail = inbox(&mut conn, &will);
        assert_eq!(
            will_mail
                .iter()
                .filter(|m| m.body == "same" && m.auto_reply == Some(true))
                .count(),
            1
        );
    }

    #[test]
    fn side_channels_lifecycle_generation_bounds_and_exclusion() {
        let (mut conn, will, [mateo, zed, lu]) = board();
        let begin = |conn: &mut Connection, agent: &Caller, g: &str| {
            begin_side_session(
                conn,
                agent,
                proto::BeginSideSessionRequest {
                    generation: g.into(),
                },
            )
        };
        let question = |id: &str, required: bool| proto::InputQuestion {
            id: id.into(),
            header: format!("H{id}"),
            text: format!("Pick {id}"),
            options: vec![
                proto::InputOption {
                    value: "a".into(),
                    label: Some("A".into()),
                    description: None,
                },
                proto::InputOption {
                    value: "b".into(),
                    label: None,
                    description: Some("the b".into()),
                },
            ],
            allow_free_text: false,
            required,
        };
        let ask = |conn: &mut Connection, g: &str, seq: i64, qs| {
            request_owner_input(
                conn,
                &mateo,
                proto::RequestOwnerInputRequest {
                    generation: g.into(),
                    request_seq: seq,
                    questions: qs,
                    message: None,
                    is_blocking: false,
                },
            )
        };
        let mail_counts = |conn: &mut Connection| -> (i64, i64, i64) {
            (
                conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
                    .unwrap(),
                conn.query_row("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                    .unwrap(),
                conn.query_row("SELECT count(*) FROM agent_human_conversations", [], |r| {
                    r.get(0)
                })
                .unwrap(),
            )
        };
        let before = mail_counts(&mut conn);

        // Generation validation and idempotent begin.
        assert!(begin(&mut conn, &mateo, "").is_err());
        assert!(begin(&mut conn, &mateo, &"g".repeat(129)).is_err());
        let first = begin(&mut conn, &mateo, "run-1").unwrap();
        assert_eq!(first.generation, "run-1");
        // Reporting with a generation never begun is stale.
        let err = report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                generation: "never".into(),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("stale side generation"), "{err}");
        // Usage validation: source required, limit positive.
        for usage in [
            proto::ContextUsage {
                used_tokens: Some(10),
                limit_tokens: Some(100),
                source: 0,
            },
            proto::ContextUsage {
                used_tokens: Some(10),
                limit_tokens: Some(0),
                source: 1,
            },
        ] {
            assert!(
                report_context(
                    &mut conn,
                    &mateo,
                    proto::ReportContextRequest {
                        side_followups: None,
                        generation: "run-1".into(),
                        usage: Some(usage),
                        side_supported: None,
                        side_reason: None,
                        preserve_usage: false,
                    },
                )
                .is_err()
            );
        }
        report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-1".into(),
                usage: Some(proto::ContextUsage {
                    used_tokens: Some(120),
                    limit_tokens: Some(100),
                    source: proto::context_usage::Source::CurrentContext.into(),
                }),
                side_supported: None,
                side_reason: None,
                preserve_usage: false,
            },
        )
        .unwrap();
        // An idempotent re-begin keeps state: lost ACK never resets.
        begin(&mut conn, &mateo, "run-1").unwrap();
        let report = side_report(
            &mut conn,
            &mateo,
            proto::SideReportRequest {
                agent: String::new(),
                generation: Some("run-1".into()),
            },
        )
        .unwrap();
        let usage = report.usage.unwrap();
        assert_eq!(
            (usage.used_tokens, usage.limit_tokens, usage.source()),
            (
                Some(120),
                Some(100),
                proto::context_usage::Source::CurrentContext
            )
        );

        // Bounds.
        assert!(ask(&mut conn, "run-1", 1, vec![]).is_err());
        assert!(ask(&mut conn, "run-1", 1, vec![question("a", true); 4].to_vec()).is_err());
        let mut many = question("a", true);
        many.options = (0..17)
            .map(|n| proto::InputOption {
                value: format!("v{n}"),
                label: None,
                description: None,
            })
            .collect();
        assert!(ask(&mut conn, "run-1", 1, vec![many]).is_err());
        let mut big = question("a", true);
        big.text = "x".repeat(4 * 1024 + 1);
        assert!(ask(&mut conn, "run-1", 1, vec![big]).is_err());
        let dup = vec![question("a", true), question("a", false)];
        assert!(ask(&mut conn, "run-1", 1, dup).is_err());

        // Ask, and one pending only. The form-level message is preserved
        // verbatim and capped.
        let err = request_owner_input(
            &mut conn,
            &mateo,
            proto::RequestOwnerInputRequest {
                generation: "run-1".into(),
                request_seq: 1,
                questions: vec![question("a", true)],
                message: Some("x".repeat(4 * 1024 + 1)),
                is_blocking: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("form message"), "{err}");
        let request = request_owner_input(
            &mut conn,
            &mateo,
            proto::RequestOwnerInputRequest {
                generation: "run-1".into(),
                request_seq: 1,
                questions: vec![question("a", true), question("b", false)],
                message: Some("Deploy the release candidate?".into()),
                is_blocking: false,
            },
        )
        .unwrap();
        let err = ask(&mut conn, "run-1", 2, vec![question("c", true)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already pending"), "{err}");
        // A lost publish reply: the same sequence and form returns the same id.
        let retry = request_owner_input(
            &mut conn,
            &mateo,
            proto::RequestOwnerInputRequest {
                generation: "run-1".into(),
                request_seq: 1,
                questions: vec![question("a", true), question("b", false)],
                message: Some("Deploy the release candidate?".into()),
                is_blocking: false,
            },
        )
        .unwrap();
        assert_eq!(retry.request_id, request.request_id);
        // Same sequence, different form; and an older sequence: rejected.
        let err = ask(&mut conn, "run-1", 1, vec![question("z", true)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("different form"), "{err}");
        let err = ask(&mut conn, "run-1", 0, vec![question("z", true)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("positive"), "{err}");

        // Owner scoping: another human cannot answer or read.
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        let ada = Caller {
            human: Some("ada".into()),
            ..Default::default()
        };
        let err = answer_owner_input(
            &mut conn,
            &ada,
            proto::AnswerOwnerInputRequest {
                request_id: request.request_id,
                action: proto::AnswerAction::Cancel.into(),
                answers: vec![],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not your agent"), "{err}");
        let err = side_report(
            &mut conn,
            &ada,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not your agent"), "{err}");

        // Answer validation: unknown question, non-option value, required miss.
        let mut bad = |answers: Vec<proto::InputAnswer>| {
            answer_owner_input(
                &mut conn,
                &will,
                proto::AnswerOwnerInputRequest {
                    request_id: request.request_id,
                    action: proto::AnswerAction::Submit.into(),
                    answers,
                },
            )
            .unwrap_err()
            .to_string()
        };
        assert!(
            bad(vec![proto::InputAnswer {
                question_id: "zzz".into(),
                value: "a".into(),
            }])
            .contains("unknown question")
        );
        assert!(
            bad(vec![proto::InputAnswer {
                question_id: "a".into(),
                value: "off-script".into(),
            }])
            .contains("not an offered option")
        );
        assert!(bad(vec![]).contains("required question unanswered"));

        // Owner submits; an optional question may be skipped.
        answer_owner_input(
            &mut conn,
            &will,
            proto::AnswerOwnerInputRequest {
                request_id: request.request_id,
                action: proto::AnswerAction::Submit.into(),
                answers: vec![proto::InputAnswer {
                    question_id: "a".into(),
                    value: "b".into(),
                }],
            },
        )
        .unwrap();
        let report = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        let input = report.pending_input.unwrap();
        assert_eq!(input.status(), proto::InputStatus::Answered);
        assert_eq!(input.answers.len(), 1);
        assert_eq!(input.answers[0].value, "b");
        assert_eq!(
            input.message.as_deref(),
            Some("Deploy the release candidate?")
        );

        // The driver closes its own next request; a fresh generation resets.
        let second = ask(&mut conn, "run-1", 2, vec![question("c", true)]).unwrap();
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-1".into(),
                request_id: Some(second.request_id),
                request_seq: None,
            },
        )
        .unwrap();
        let third = ask(&mut conn, "run-1", 3, vec![question("d", true)]).unwrap();
        assert!(third.request_id > second.request_id);
        begin(&mut conn, &mateo, "run-2").unwrap();
        // The reset staled the pending request and cleared usage.
        let report = side_report(
            &mut conn,
            &mateo,
            proto::SideReportRequest {
                agent: String::new(),
                generation: Some("run-2".into()),
            },
        )
        .unwrap();
        assert!(report.usage.is_none());
        assert_eq!(
            report.pending_input.unwrap().status(),
            proto::InputStatus::Stale
        );
        // The old generation is now rejected everywhere.
        for err in [
            report_context(
                &mut conn,
                &mateo,
                proto::ReportContextRequest {
                    generation: "run-1".into(),
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string(),
            ask(&mut conn, "run-1", 1, vec![question("e", true)])
                .unwrap_err()
                .to_string(),
        ] {
            assert!(err.contains("stale side generation"), "{err}");
        }
        // Another agent's generation is independent.
        assert!(begin(&mut conn, &zed, "zed-1").is_ok());
        let _ = lu;

        // The closed-sequence barrier retires requests that never arrived.
        // Nonpositive sequences reject, like publish.
        begin(&mut conn, &mateo, "run-3").unwrap();
        for seq in [0, -1] {
            let err = close_owner_input(
                &mut conn,
                &mateo,
                proto::CloseOwnerInputRequest {
                    generation: "run-3".into(),
                    request_id: None,
                    request_seq: Some(seq),
                },
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("must be positive"), "{seq}: {err}");
        }
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-3".into(),
                request_id: None,
                request_seq: Some(2),
            },
        )
        .unwrap();
        for seq in [1, 2] {
            let err = ask(&mut conn, "run-3", seq, vec![question("q", true)])
                .unwrap_err()
                .to_string();
            assert!(err.contains("sequence is closed"), "{seq}: {err}");
        }
        let open = ask(&mut conn, "run-3", 3, vec![question("q", true)]).unwrap();
        assert!(open.request_id > 0);
        // Close by sequence also cancels the matching pending row.
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-3".into(),
                request_id: None,
                request_seq: Some(3),
            },
        )
        .unwrap();
        let report = side_report(
            &mut conn,
            &mateo,
            proto::SideReportRequest {
                agent: String::new(),
                generation: Some("run-3".into()),
            },
        )
        .unwrap();
        assert_eq!(
            report.pending_input.unwrap().status(),
            proto::InputStatus::Cancelled
        );
        // A new request prunes the terminal one: latest-only storage.
        let rows_before: i64 = conn
            .query_row("SELECT count(*) FROM owner_input_requests", [], |r| {
                r.get(0)
            })
            .unwrap();
        ask(&mut conn, "run-3", 4, vec![question("q", true)]).unwrap();
        let rows_after: i64 = conn
            .query_row("SELECT count(*) FROM owner_input_requests", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows_after, rows_before);

        // Form validity: no way to answer; decline/cancel carrying answers.
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-3".into(),
                request_id: None,
                request_seq: Some(4),
            },
        )
        .unwrap();
        let mut impossible = question("x", true);
        impossible.options = vec![];
        impossible.allow_free_text = false;
        let err = ask(&mut conn, "run-3", 5, vec![impossible])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no way to answer"), "{err}");
        let fifth = ask(&mut conn, "run-3", 5, vec![question("q", true)]).unwrap();
        let err = answer_owner_input(
            &mut conn,
            &will,
            proto::AnswerOwnerInputRequest {
                request_id: fifth.request_id,
                action: proto::AnswerAction::Cancel.into(),
                answers: vec![proto::InputAnswer {
                    question_id: "q".into(),
                    value: "a".into(),
                }],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only accompany a submit"), "{err}");

        // is_blocking round-trips verbatim.
        begin(&mut conn, &mateo, "run-4").unwrap();
        report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-4".into(),
                usage: Some(proto::ContextUsage {
                    used_tokens: Some(9),
                    limit_tokens: None,
                    source: proto::context_usage::Source::LastModelRequest.into(),
                }),
                side_supported: None,
                side_reason: None,
                preserve_usage: false,
            },
        )
        .unwrap();
        let report = side_report(
            &mut conn,
            &mateo,
            proto::SideReportRequest {
                agent: String::new(),
                generation: Some("run-4".into()),
            },
        )
        .unwrap();
        assert!(report.usage.is_some());
        let blocking = request_owner_input(
            &mut conn,
            &mateo,
            proto::RequestOwnerInputRequest {
                generation: "run-4".into(),
                request_seq: 1,
                questions: vec![question("a", true)],
                message: None,
                is_blocking: true,
            },
        )
        .unwrap();
        let report = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        assert!(report.pending_input.unwrap().is_blocking);

        // A successful close is idempotent after a lost ACK; other terminal
        // states still reject.
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-4".into(),
                request_id: Some(blocking.request_id),
                request_seq: None,
            },
        )
        .unwrap();
        close_owner_input(
            &mut conn,
            &mateo,
            proto::CloseOwnerInputRequest {
                generation: "run-4".into(),
                request_id: Some(blocking.request_id),
                request_seq: None,
            },
        )
        .unwrap();

        // Nothing side-channel ever becomes mail, a delivery, or a conversation.
        assert_eq!(mail_counts(&mut conn), before);
    }

    #[test]
    fn side_conversations_follow_end_and_derive_ended() {
        let (mut conn, will, [mateo, zed, _lu]) = board();
        let begin = |conn: &mut Connection, g: &str| {
            begin_side_session(
                conn,
                &mateo,
                proto::BeginSideSessionRequest {
                    generation: g.into(),
                },
            )
            .unwrap();
        };
        // Every side row and each agent's revision, to show a call wrote nothing.
        let snapshot = |conn: &mut Connection| -> Vec<String> {
            let mut out: Vec<String> = conn
                .prepare(
                    "SELECT id, agent_id, generation, status, answer, ended, updated_at
                     FROM side_requests ORDER BY id",
                )
                .unwrap()
                .query_map([], |r| {
                    Ok(format!(
                        "{}|{}|{:?}|{}|{:?}|{}|{}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let revisions: Vec<String> = conn
                .prepare("SELECT agent_id, revision FROM side_state ORDER BY agent_id")
                .unwrap()
                .query_map([], |r| {
                    Ok(format!("{}:{}", r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            out.extend(revisions);
            out
        };
        let support = |conn: &mut Connection, g: &str, followups: Option<bool>| {
            report_context(
                conn,
                &mateo,
                proto::ReportContextRequest {
                    side_followups: followups,
                    generation: g.into(),
                    usage: None,
                    side_supported: Some(true),
                    side_reason: None,
                    preserve_usage: false,
                },
            )
            .unwrap();
        };
        let ask = |conn: &mut Connection, q: &str, follows: Option<i64>| {
            ask_side(
                conn,
                &will,
                proto::AskSideRequest {
                    follows,
                    agent: "Mateo".into(),
                    question: q.into(),
                },
            )
            .map(|r| r.id)
            .map_err(|e| e.to_string())
        };
        let claim = |conn: &mut Connection, g: &str| {
            claim_side(
                conn,
                &mateo,
                proto::ClaimSideRequest {
                    generation: g.into(),
                },
            )
            .unwrap()
            .request
            .unwrap()
        };
        let report = |conn: &mut Connection, g: &str, id: i64, status: proto::SideStatus| {
            report_side(
                conn,
                &mateo,
                proto::ReportSideRequest {
                    generation: g.into(),
                    id,
                    status: status.into(),
                    answer: Some("answer".into()),
                },
            )
            .map_err(|e| e.to_string())
        };
        let cancel = |conn: &mut Connection, id: i64, end: bool| {
            cancel_side(conn, &will, proto::CancelSideRequest { id, end })
                .map_err(|e| e.to_string())
        };
        let view = |conn: &mut Connection| {
            side_report(
                conn,
                &will,
                proto::SideReportRequest {
                    agent: "Mateo".into(),
                    generation: None,
                },
            )
            .unwrap()
        };
        let latest = |conn: &mut Connection| view(conn).side.unwrap();
        // Ask, claim and complete a fresh question on `g`.
        let answered = |conn: &mut Connection, g: &str, q: &str, follows: Option<i64>| {
            let id = ask(conn, q, follows).unwrap();
            assert_eq!(claim(conn, g).id, id);
            report(conn, g, id, proto::SideStatus::SideCompleted).unwrap();
            id
        };
        let mail = |conn: &mut Connection| -> i64 {
            conn.query_row(
                "SELECT (SELECT count(*) FROM messages) + (SELECT count(*) FROM deliveries)
                   + (SELECT count(*) FROM agent_human_conversations)",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let before = mail(&mut conn);
        let ended = "side conversation ended";

        // A first question is its own conversation; open and unclaimed, it
        // is not ended. Follow-ups need the runner to say it continues them.
        begin(&mut conn, "run-1");
        support(&mut conn, "run-1", None);
        let first = ask(&mut conn, "q1", None).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.conversation, row.follows, row.ended),
            (Some(first), None, false)
        );
        assert_eq!(claim(&mut conn, "run-1").conversation, Some(first));
        report(&mut conn, "run-1", first, proto::SideStatus::SideCompleted).unwrap();
        let err = ask(&mut conn, "q2", Some(first)).unwrap_err();
        assert!(err.contains("need an updated runner"), "{err}");
        support(&mut conn, "run-1", Some(true));
        assert_eq!(view(&mut conn).side_followups, Some(true));
        // Omitted preserves it.
        support(&mut conn, "run-1", None);
        assert_eq!(view(&mut conn).side_followups, Some(true));

        // Only the latest question continues; a stale or unknown id is refused.
        assert!(
            ask(&mut conn, "q2", Some(first + 100))
                .unwrap_err()
                .contains(ended)
        );
        let second = ask(&mut conn, "q2", Some(first)).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.conversation, row.follows, row.ended),
            (Some(first), Some(first), false)
        );
        // A second follow-up of the same id is refused while the first is
        // open, and once it is answered the old id is no longer the latest.
        assert!(
            ask(&mut conn, "q2b", Some(first))
                .unwrap_err()
                .contains("already open")
        );
        assert_eq!(claim(&mut conn, "run-1").follows, Some(first));
        report(&mut conn, "run-1", second, proto::SideStatus::SideCompleted).unwrap();
        assert!(
            ask(&mut conn, "q2b", Some(first))
                .unwrap_err()
                .contains(ended)
        );

        // ENDED for an open follow-up the runner cannot continue: terminal.
        let third = ask(&mut conn, "q3", Some(second)).unwrap();
        claim(&mut conn, "run-1");
        report(&mut conn, "run-1", third, proto::SideStatus::SideEnded).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideEnded, true)
        );
        assert!(
            ask(&mut conn, "q4", Some(third))
                .unwrap_err()
                .contains(ended)
        );
        // An open first question cannot be reported ENDED.
        let fresh = ask(&mut conn, "fresh", None).unwrap();
        assert_eq!(latest(&mut conn).conversation, Some(fresh));
        claim(&mut conn, "run-1");
        let err = report(&mut conn, "run-1", fresh, proto::SideStatus::SideEnded).unwrap_err();
        assert!(err.contains("only a follow-up"), "{err}");
        report(&mut conn, "run-1", fresh, proto::SideStatus::SideCompleted).unwrap();

        // A lost child on the latest terminal question, with or without
        // follows, flags it ended and keeps its status; a repeat is a no-op.
        report(&mut conn, "run-1", fresh, proto::SideStatus::SideEnded).unwrap();
        let flagged = snapshot(&mut conn);
        report(&mut conn, "run-1", fresh, proto::SideStatus::SideEnded).unwrap();
        assert_eq!(snapshot(&mut conn), flagged);
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideCompleted, true)
        );
        assert!(
            ask(&mut conn, "more", Some(fresh))
                .unwrap_err()
                .contains(ended)
        );
        let one = answered(&mut conn, "run-1", "one", None);
        let two = answered(&mut conn, "run-1", "two", Some(one));
        report(&mut conn, "run-1", two, proto::SideStatus::SideEnded).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideCompleted, true)
        );
        // A question a newer one replaced (and pruned) is a no-op too: the
        // current row, its status, answer and the revision stay.
        answered(&mut conn, "run-1", "replacing", None);
        let current = snapshot(&mut conn);
        report(&mut conn, "run-1", two, proto::SideStatus::SideEnded).unwrap();
        assert_eq!(snapshot(&mut conn), current);
        assert!(!latest(&mut conn).ended);

        // Another agent's existing question is refused, not a no-op, and
        // left as it was, even though Mateo has a newer one. Zed uses the
        // same generation string, so only the agent check refuses it.
        begin_side_session(
            &mut conn,
            &zed,
            proto::BeginSideSessionRequest {
                generation: "run-1".into(),
            },
        )
        .unwrap();
        let zed_row: i64 = conn
            .query_row(
                "INSERT INTO side_requests
                   (agent_id, generation, question, asked_at, status, answer, updated_at, conversation)
                 SELECT id, 'run-1', 'z', 't', 'completed', 'za', 't', 0 FROM agents WHERE name = 'Zed'
                 RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let theirs = snapshot(&mut conn);
        let err = report(&mut conn, "run-1", zed_row, proto::SideStatus::SideEnded).unwrap_err();
        assert!(err.contains("not your claimed"), "{err}");
        assert_eq!(snapshot(&mut conn), theirs);
        let newer = answered(&mut conn, "run-1", "newer", None);

        // Idle past the limit, the latest terminal question is ended: derived
        // from the clock at read time, with no write to the revision. (The
        // test moves the row's time back instead of the clock forward.)
        let stale = fmt_time(Utc::now() - SIDE_IDLE_LIMIT - chrono::TimeDelta::seconds(1));
        let revision = view(&mut conn).revision;
        assert!(!latest(&mut conn).ended);
        conn.execute(
            "UPDATE side_requests SET updated_at = ?2 WHERE id = ?1",
            rusqlite::params![newer, stale],
        )
        .unwrap();
        assert!(latest(&mut conn).ended);
        assert_eq!(view(&mut conn).revision, revision);
        assert!(
            ask(&mut conn, "late", Some(newer))
                .unwrap_err()
                .contains(ended)
        );

        // A restart ends what the old generation answered, even once the new
        // one says it continues conversations; it starts without followups.
        let before_restart = answered(&mut conn, "run-1", "before", None);
        assert!(!latest(&mut conn).ended);
        begin(&mut conn, "run-2");
        support(&mut conn, "run-2", None);
        assert_eq!(view(&mut conn).side_followups, None);
        support(&mut conn, "run-2", Some(true));
        assert!(latest(&mut conn).ended);
        assert!(
            ask(&mut conn, "after", Some(before_restart))
                .unwrap_err()
                .contains(ended)
        );
        // The stale generation cannot report on it.
        assert!(
            report(
                &mut conn,
                "run-1",
                before_restart,
                proto::SideStatus::SideEnded
            )
            .is_err()
        );

        // The owner ends a terminal conversation: status kept, idempotent.
        let done = answered(&mut conn, "run-2", "done", None);
        cancel(&mut conn, done, true).unwrap();
        cancel(&mut conn, done, true).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideCompleted, true)
        );
        assert!(
            ask(&mut conn, "next", Some(done))
                .unwrap_err()
                .contains(ended)
        );
        // Ending an open question also cancels it; only the latest can be ended.
        let open = ask(&mut conn, "open", None).unwrap();
        cancel(&mut conn, open, true).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideCancelled, true)
        );
        let after = answered(&mut conn, "run-2", "after", None);
        assert!(cancel(&mut conn, open, true).is_err());
        // Cancelling one turn keeps the conversation going.
        let turn = ask(&mut conn, "turn", Some(after)).unwrap();
        claim(&mut conn, "run-2");
        cancel(&mut conn, turn, false).unwrap();
        let row = latest(&mut conn);
        assert_eq!(
            (row.status(), row.ended),
            (proto::SideStatus::SideCancelled, false)
        );
        let again = ask(&mut conn, "again", Some(turn)).unwrap();
        assert_eq!(latest(&mut conn).conversation, Some(after));
        cancel(&mut conn, again, true).unwrap();
        // Cancelled before any runner claimed it: no child, so ended.
        let unclaimed = ask(&mut conn, "unclaimed", None).unwrap();
        cancel(&mut conn, unclaimed, false).unwrap();
        assert!(latest(&mut conn).ended);
        assert!(
            ask(&mut conn, "never", Some(unclaimed))
                .unwrap_err()
                .contains(ended)
        );

        // None of it is mail.
        assert_eq!(mail(&mut conn), before);
    }

    #[test]
    fn side_questions_lifecycle_claim_cancel_reset_and_exclusion() {
        let (mut conn, will, [mateo, _zed, _lu]) = board();
        let begin = |conn: &mut Connection, g: &str| {
            begin_side_session(
                conn,
                &mateo,
                proto::BeginSideSessionRequest {
                    generation: g.into(),
                },
            )
        };
        let support = |conn: &mut Connection, g: &str, ok: bool| {
            report_context(
                conn,
                &mateo,
                proto::ReportContextRequest {
                    side_followups: None,
                    generation: g.into(),
                    usage: None,
                    side_supported: Some(ok),
                    side_reason: None,
                    preserve_usage: false,
                },
            )
        };
        let ask = |conn: &mut Connection, q: &str| {
            ask_side(
                conn,
                &will,
                proto::AskSideRequest {
                    follows: None,
                    agent: "Mateo".into(),
                    question: q.into(),
                },
            )
        };
        let mail_counts = |conn: &mut Connection| -> (i64, i64, i64) {
            (
                conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
                    .unwrap(),
                conn.query_row("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                    .unwrap(),
                conn.query_row("SELECT count(*) FROM agent_human_conversations", [], |r| {
                    r.get(0)
                })
                .unwrap(),
            )
        };
        let before = mail_counts(&mut conn);

        // The support gate: nothing reported, ask fails; support reported, it works.
        begin(&mut conn, "run-1").unwrap();
        let err = ask(&mut conn, "hi").unwrap_err().to_string();
        assert!(err.contains("does not answer side questions"), "{err}");
        support(&mut conn, "run-1", true).unwrap();
        // Bounds and one-open.
        assert!(ask(&mut conn, "").is_err());
        assert!(ask(&mut conn, &"x".repeat(4 * 1024 + 1)).is_err());
        let first = ask(&mut conn, "what broke?").unwrap();
        let err = ask(&mut conn, "another").unwrap_err().to_string();
        assert!(err.contains("already open"), "{err}");

        // Claim: empty for others, once per generation, retry-safe.
        let nothing = claim_side(
            &mut conn,
            &_zed,
            proto::ClaimSideRequest {
                generation: "never".into(),
            },
        );
        assert!(nothing.is_err(), "zed never began a session");
        let claim = |conn: &mut Connection, g: &str| {
            claim_side(
                conn,
                &mateo,
                proto::ClaimSideRequest {
                    generation: g.into(),
                },
            )
        };
        let got = claim(&mut conn, "run-1").unwrap().request.unwrap();
        assert_eq!(got.id, first.id);
        // A lost reply's retry returns the same claim, not a fresh invocation.
        let again = claim(&mut conn, "run-1").unwrap().request.unwrap();
        assert_eq!(again.id, first.id);

        // Progressive then terminal, only by the claiming generation.
        let report = |conn: &mut Connection, g: &str, status: proto::SideStatus, answer: &str| {
            report_side(
                conn,
                &mateo,
                proto::ReportSideRequest {
                    generation: g.into(),
                    id: first.id,
                    status: status.into(),
                    answer: Some(answer.into()),
                },
            )
        };
        let err = report(&mut conn, "run-1", proto::SideStatus::Unspecified, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OPEN, COMPLETED, FAILED or ENDED"), "{err}");
        let err = report(
            &mut conn,
            "run-1",
            proto::SideStatus::SideOpen,
            &"x".repeat(64 * 1024 + 1),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("64 KiB"), "{err}");
        report(&mut conn, "run-1", proto::SideStatus::SideOpen, "looking\n").unwrap();
        let view = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        assert_eq!(view.side.unwrap().answer.as_deref(), Some("looking\n"));
        // The owner cancels; a late terminal report loses.
        cancel_side(
            &mut conn,
            &will,
            proto::CancelSideRequest {
                id: first.id,
                end: false,
            },
        )
        .unwrap();
        // Cancel is idempotent for the exact id.
        cancel_side(
            &mut conn,
            &will,
            proto::CancelSideRequest {
                id: first.id,
                end: false,
            },
        )
        .unwrap();
        let err = report(&mut conn, "run-1", proto::SideStatus::SideCompleted, "done")
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed"), "{err}");

        // A report names its registration: another agent holding the same
        // generation string cannot touch it.
        begin_side_session(
            &mut conn,
            &_zed,
            proto::BeginSideSessionRequest {
                generation: "run-1".into(),
            },
        )
        .unwrap();
        let err = report_side(
            &mut conn,
            &_zed,
            proto::ReportSideRequest {
                generation: "run-1".into(),
                id: first.id,
                status: proto::SideStatus::SideCompleted.into(),
                answer: Some("not mine".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not your claimed"), "{err}");
        // Ownership precedes cancel idempotence: a cancelled id from another
        // human's agent still rejects.
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        let ada = Caller {
            human: Some("ada".into()),
            ..Default::default()
        };
        let err = cancel_side(
            &mut conn,
            &ada,
            proto::CancelSideRequest {
                id: first.id,
                end: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not your agent"), "{err}");

        // A fresh generation interrupts everything open and clears capability.
        let second = ask(&mut conn, "unclaimed").unwrap();
        let third = ask(&mut conn, "claimed").unwrap_err(); // still one open
        assert!(third.to_string().contains("already open"));
        // Claim it, then also check an unclaimed one after replacement.
        let got = claim(&mut conn, "run-1").unwrap().request.unwrap();
        assert_eq!(got.id, second.id);
        begin(&mut conn, "run-2").unwrap();
        let view = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        assert_eq!(
            view.side.unwrap().status(),
            proto::SideStatus::SideInterrupted
        );
        assert_eq!(view.side_supported, None);
        let err = ask(&mut conn, "again").unwrap_err().to_string();
        assert!(err.contains("does not answer side questions"), "{err}");
        // Omitted capability fields preserve; an explicit report restores.
        support(&mut conn, "run-2", true).unwrap();
        report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-2".into(),
                usage: None,
                side_supported: None,
                side_reason: None,
                preserve_usage: false,
            },
        )
        .unwrap();
        assert!(ask(&mut conn, "again").is_ok());

        // preserve_usage: capability-only reports leave usage and its report
        // time untouched; it never takes new usage.
        report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-2".into(),
                usage: Some(proto::ContextUsage {
                    used_tokens: Some(42),
                    limit_tokens: Some(100),
                    source: proto::context_usage::Source::CurrentContext.into(),
                }),
                side_supported: None,
                side_reason: None,
                preserve_usage: false,
            },
        )
        .unwrap();
        // Pin the report time to a fixed old instant, so preservation can
        // never pass by the clock merely standing still.
        conn.execute(
            "UPDATE side_state SET reported_at = '2020-01-01T00:00:00.000000Z' WHERE agent_id = 1",
            [],
        )
        .unwrap();
        let stamp = "2020-01-01T00:00:00.000000Z";
        let err = report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-2".into(),
                usage: Some(proto::ContextUsage {
                    used_tokens: Some(1),
                    limit_tokens: None,
                    source: proto::context_usage::Source::LastModelRequest.into(),
                }),
                side_supported: None,
                side_reason: None,
                preserve_usage: true,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not take new usage"), "{err}");
        report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                side_followups: None,
                generation: "run-2".into(),
                usage: None,
                side_supported: Some(false),
                side_reason: Some("child setup failed".into()),
                preserve_usage: true,
            },
        )
        .unwrap();
        let view = side_report(
            &mut conn,
            &mateo,
            proto::SideReportRequest {
                agent: String::new(),
                generation: Some("run-2".into()),
            },
        )
        .unwrap();
        assert_eq!(view.usage.unwrap().used_tokens, Some(42));
        assert_eq!(view.reported_at, stamp.to_string());
        assert_eq!(view.side_supported, Some(false));
        assert_eq!(view.side_reason.as_deref(), Some("child setup failed"));

        // Nothing side-channel ever becomes mail, a delivery, or a conversation.
        assert_eq!(mail_counts(&mut conn), before);

        // Retirement cleans side questions; re-registration starts empty.
        retire_agent(
            &mut conn,
            &will,
            proto::RetireRequest {
                agent: "Mateo".into(),
            },
        )
        .unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM side_requests WHERE agent_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        let token = register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let mateo2 = Caller {
            token: Some(token),
            ..Default::default()
        };
        let view = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        assert!(view.side.is_none());
        let _ = mateo2;
    }

    #[test]
    fn side_channels_clear_on_retirement_and_re_registration() {
        let (mut conn, will, [mateo, _zed, _lu]) = board();
        let begin = |conn: &mut Connection, agent: &Caller, g: &str| {
            begin_side_session(
                conn,
                agent,
                proto::BeginSideSessionRequest {
                    generation: g.into(),
                },
            )
        };
        let question = |id: &str| proto::InputQuestion {
            id: id.into(),
            header: String::new(),
            text: "Pick".into(),
            options: vec![],
            allow_free_text: true,
            required: true,
        };
        begin(&mut conn, &mateo, "run-1").unwrap();
        let pending = request_owner_input(
            &mut conn,
            &mateo,
            proto::RequestOwnerInputRequest {
                generation: "run-1".into(),
                request_seq: 1,
                questions: vec![question("b")],
                message: None,
                is_blocking: false,
            },
        )
        .unwrap();
        retire_agent(
            &mut conn,
            &will,
            proto::RetireRequest {
                agent: "Mateo".into(),
            },
        )
        .unwrap();
        let err = answer_owner_input(
            &mut conn,
            &will,
            proto::AnswerOwnerInputRequest {
                request_id: pending.request_id,
                action: proto::AnswerAction::Submit.into(),
                answers: vec![proto::InputAnswer {
                    question_id: "b".into(),
                    value: "a".into(),
                }],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("retired") || err.contains("no such input request"),
            "{err}"
        );
        let mateo_rows: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM side_state WHERE agent_id = 1) + (SELECT count(*) FROM owner_input_requests WHERE agent_id = 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mateo_rows, 0);
        let token = register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let mateo2 = Caller {
            token: Some(token),
            ..Default::default()
        };
        let report = side_report(
            &mut conn,
            &will,
            proto::SideReportRequest {
                agent: "Mateo".into(),
                generation: None,
            },
        )
        .unwrap();
        assert!(report.pending_input.is_none() && report.usage.is_none());
        assert!(begin(&mut conn, &mateo2, "fresh-1").is_ok());
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
    fn conversation_group_feed_uses_deliveries_at_send_time_without_changing_default() {
        let (mut conn, will, [mateo, _, lu]) = board();
        let post = |conn: &mut Connection, to: &str, body: &str| {
            send(
                conn,
                &will,
                proto::SendRequest {
                    to: to.into(),
                    body: body.into(),
                    control: None,
                },
            )
            .unwrap();
        };
        post(&mut conn, "@crew", "before join");
        group_join(
            &mut conn,
            &lu,
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        post(&mut conn, "@crew", "while joined");
        post(&mut conn, "Lu", "direct");
        group_leave(
            &mut conn,
            &lu,
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        post(&mut conn, "@crew", "after leave");
        let req = proto::FeedRequest {
            backlog: 100,
            with: Some("lu".into()),
            include_group_deliveries: true,
            ..Default::default()
        };
        let enriched = feed(&mut conn, &will, req.clone()).unwrap().messages;
        let bodies: Vec<_> = enriched.iter().map(|m| m.body.as_str()).collect();
        assert!(
            bodies.contains(&"while joined") && bodies.contains(&"direct"),
            "{bodies:?}"
        );
        assert!(
            !bodies.contains(&"before join") && !bodies.contains(&"after leave"),
            "{bodies:?}"
        );
        let ids: std::collections::HashSet<_> = enriched.iter().map(|m| m.id).collect();
        assert_eq!(
            ids.len(),
            enriched.len(),
            "sent and delivered union must not duplicate notices"
        );
        let old = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                include_group_deliveries: false,
                ..req.clone()
            },
        )
        .unwrap()
        .messages;
        assert!(old.iter().any(|m| m.body == "direct"));
        assert!(!old.iter().any(|m| m.body == "while joined"));
        // One-row cursor pages and recent windows must not drop/duplicate broadcasts.
        let mut cursor = 0;
        let mut paged = Vec::new();
        loop {
            let page = feed(
                &mut conn,
                &will,
                proto::FeedRequest {
                    backlog: 1,
                    after_id: Some(cursor),
                    ..req.clone()
                },
            )
            .unwrap()
            .messages;
            let Some(message) = page.first() else { break };
            cursor = message.id;
            paged.push(cursor);
        }
        assert_eq!(paged, enriched.iter().map(|m| m.id).collect::<Vec<_>>());
        let recent = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                backlog: 1,
                ..req.clone()
            },
        )
        .unwrap()
        .messages;
        assert_eq!(recent[0].id, *paged.last().unwrap());
        let grouped = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                group: Some("crew".into()),
                ..req.clone()
            },
        )
        .unwrap()
        .messages;
        assert!(
            grouped
                .iter()
                .all(|m| m.to_group.as_deref() == Some("crew"))
        );
        assert!(!grouped.iter().any(|m| m.body == "direct"));
        // A human named by `with` still means direct mail, not their broadcast inbox.
        let human = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                with: Some("will".into()),
                ..req.clone()
            },
        )
        .unwrap()
        .messages;
        let human_old = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                with: Some("will".into()),
                include_group_deliveries: false,
                ..req.clone()
            },
        )
        .unwrap()
        .messages;
        assert_eq!(human, human_old);
        assert!(
            feed(&mut conn, &mateo, req.clone()).is_err(),
            "feed stays human-only"
        );
        assert!(feed(&mut conn, &will, proto::FeedRequest { with: None, ..req }).is_err());
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
        assert!(
            s.iter()
                .all(|s| s.unread_messages == 0 && s.unseen_stream == 0)
        );
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

    fn sync_req(after: i64, backlog: u32) -> proto::FeedRequest {
        proto::FeedRequest {
            backlog,
            mine: true,
            after_id: Some(after),
            sync: true,
            ..Default::default()
        }
    }

    fn say(conn: &mut Connection, from: &Caller, to: &str, body: &str) -> i64 {
        send(
            conn,
            from,
            proto::SendRequest {
                to: to.into(),
                body: body.into(),
                control: None,
            },
        )
        .unwrap();
        // Broadcasts report no id; the message is the newest row.
        newest(conn)
    }

    /// The newest message id so far: the board's own setup writes some.
    fn newest(conn: &Connection) -> i64 {
        conn.query_row("SELECT coalesce(max(id), 0) FROM messages", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Every page from `req` on until one is caught up: (ids, caught_up) each.
    fn sync_pages(
        conn: &mut Connection,
        who: &Caller,
        mut req: proto::FeedRequest,
    ) -> Vec<(Vec<i64>, bool)> {
        let mut pages = Vec::new();
        loop {
            let page = feed(conn, who, req.clone()).unwrap();
            let ids = page.messages.iter().map(|m| m.id).collect();
            let more = advance_feed_cursor(&mut req, &page);
            assert_eq!(more, page.caught_up == Some(false));
            pages.push((ids, page.caught_up.unwrap()));
            if !more {
                return pages;
            }
        }
    }

    fn scopes(page: &proto::FeedSnapshot) -> Vec<(i64, Vec<&str>)> {
        page.agent_scopes
            .iter()
            .map(|s| {
                (
                    s.message_id,
                    s.agents.iter().map(|a| a.name.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn sync_pages_say_when_they_are_caught_up_even_on_an_exact_final_page() {
        let (mut conn, will, [mateo, zed, _lu]) = board();
        let base = newest(&conn);
        // Nothing yet: the empty first page is caught up.
        let empty = feed(&mut conn, &will, sync_req(base, 2)).unwrap();
        assert!(empty.messages.is_empty() && empty.agent_scopes.is_empty());
        assert_eq!(empty.caught_up, Some(true));
        let ids: Vec<i64> = (0..4)
            .map(|i| {
                say(
                    &mut conn,
                    if i % 2 == 0 { &mateo } else { &zed },
                    "will",
                    "hi",
                )
            })
            .collect();
        // Four rows in pages of two: the second is full and still caught up.
        assert_eq!(
            sync_pages(&mut conn, &will, sync_req(base, 2)),
            [(ids[..2].to_vec(), false), (ids[2..].to_vec(), true)]
        );
        // Pages of three: a short final page.
        assert_eq!(
            sync_pages(&mut conn, &will, sync_req(base, 3)),
            [(ids[..3].to_vec(), false), (ids[3..].to_vec(), true)]
        );
        // From the last row: nothing more, caught up.
        let after = feed(&mut conn, &will, sync_req(ids[3], 2)).unwrap();
        assert_eq!((after.messages.len(), after.caught_up), (0, Some(true)));
        // A write between pages is a later page's row; nothing skips.
        let mut req = sync_req(base, 2);
        let first = feed(&mut conn, &will, req.clone()).unwrap();
        assert!(advance_feed_cursor(&mut req, &first));
        let late = say(&mut conn, &mateo, "will", "late");
        let rest: Vec<i64> = sync_pages(&mut conn, &will, req)
            .into_iter()
            .flat_map(|(ids, _)| ids)
            .collect();
        assert_eq!(rest, [ids[2], ids[3], late]);
        // Ordinary feeds are unchanged: no completion and no scopes.
        let ordinary = feed(
            &mut conn,
            &will,
            proto::FeedRequest {
                sync: false,
                ..sync_req(base, 2)
            },
        )
        .unwrap();
        assert_eq!(ordinary.caught_up, None);
        assert!(ordinary.agent_scopes.is_empty());
        assert_eq!(ordinary.messages.len(), 2);
    }

    #[test]
    fn sync_scopes_are_the_callers_own_agents_as_delivered_when_sent() {
        let (mut conn, will, [_mateo, _zed, lu]) = board();
        let base = newest(&conn);
        // Lu is not in @crew yet: the broadcast reaches Mateo and Zed only.
        let crew = say(&mut conn, &will, "@crew", "to the crew");
        group_join(
            &mut conn,
            &lu,
            proto::GroupRequest {
                group: "crew".into(),
            },
        )
        .unwrap();
        let dm = say(&mut conn, &lu, "Mateo", "psst");
        let direct = say(&mut conn, &will, "Lu", "from the human");
        // Retiring keeps the history: Zed stays in its scopes.
        retire_agent(
            &mut conn,
            &will,
            proto::RetireRequest {
                agent: "Zed".into(),
            },
        )
        .unwrap();
        let page = feed(&mut conn, &will, sync_req(base, 10)).unwrap();
        let ours: Vec<_> = scopes(&page)
            .into_iter()
            .filter(|(id, _)| [crew, dm, direct].contains(id))
            .collect();
        assert_eq!(
            ours,
            [
                (crew, vec!["Mateo", "Zed"]),
                (dm, vec!["Mateo", "Lu"]),
                (direct, vec!["Lu"]),
            ]
        );
        assert_eq!(page.agent_scopes.len(), page.messages.len());
        assert!(
            page.agent_scopes
                .iter()
                .zip(&page.messages)
                .all(|(s, m)| s.message_id == m.id)
        );
    }

    #[test]
    fn sync_keeps_each_human_to_their_own_history_and_agents() {
        let (mut conn, will, [mateo, _zed, _lu]) = board();
        ensure_human(&mut conn, proto::EnsureHumanRequest { name: "ada".into() }).unwrap();
        let ada = Caller {
            human: Some("ada".into()),
            ..Default::default()
        };
        let eve = Caller {
            token: Some(
                register(
                    &mut conn,
                    &ada,
                    proto::RegisterRequest {
                        name: Some("Eve".into()),
                        ..Default::default()
                    },
                )
                .unwrap()
                .token,
            ),
            ..Default::default()
        };
        let base = newest(&conn);
        // Another human writes to will's agent: in both histories, each
        // with only its own human's agents.
        let shared = say(&mut conn, &ada, "Mateo", "between owners");
        let wills = say(&mut conn, &mateo, "will", "will's only");
        let adas = say(&mut conn, &eve, "ada", "ada's only");
        let will_page = feed(&mut conn, &will, sync_req(base, 10)).unwrap();
        assert_eq!(
            scopes(&will_page),
            [(shared, vec!["Mateo"]), (wills, vec!["Mateo"])]
        );
        let ada_page = feed(&mut conn, &ada, sync_req(base, 10)).unwrap();
        assert_eq!(scopes(&ada_page), [(shared, vec![]), (adas, vec!["Eve"])]);
        // Agents cannot replicate anyone's history.
        let refused = feed(&mut conn, &eve, sync_req(base, 10)).unwrap_err();
        assert!(
            format!("{refused:#}").contains("humans only"),
            "{refused:#}"
        );
    }

    #[test]
    fn sync_refuses_filters_a_missing_cursor_and_bad_windows() {
        let (mut conn, will, _) = board();
        let refused = |conn: &mut Connection, req: proto::FeedRequest| {
            format!("{:#}", feed(conn, &will, req).unwrap_err())
        };
        for req in [
            proto::FeedRequest {
                mine: false,
                ..sync_req(0, 10)
            },
            proto::FeedRequest {
                group: Some("crew".into()),
                ..sync_req(0, 10)
            },
            proto::FeedRequest {
                with: Some("Mateo".into()),
                include_group_deliveries: true,
                ..sync_req(0, 10)
            },
            proto::FeedRequest {
                since: Some("2026-01-01T00:00:00Z".into()),
                ..sync_req(0, 10)
            },
        ] {
            assert!(refused(&mut conn, req).contains("whole personal history"));
        }
        for after_id in [None, Some(-1)] {
            let req = proto::FeedRequest {
                after_id,
                ..sync_req(0, 10)
            };
            assert!(refused(&mut conn, req).contains("needs a cursor"));
        }
        for backlog in [0, MAX_FEED_WINDOW + 1] {
            assert!(refused(&mut conn, sync_req(0, backlog)).contains("feed window"));
        }
    }

    #[test]
    fn sync_pages_stay_within_their_byte_budget_without_cutting_a_message() {
        let (mut conn, will, [mateo, ..]) = board();
        let base = newest(&conn);
        let body = "x".repeat(MAX_BODY_BYTES);
        let ids: Vec<i64> = (0..80)
            .map(|_| say(&mut conn, &mateo, "will", &body))
            .collect();
        let mut req = sync_req(base, MAX_FEED_WINDOW);
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            let page = feed(&mut conn, &will, req.clone()).unwrap();
            pages += 1;
            assert!(prost::Message::encoded_len(&page) <= MAX_SYNC_PAGE_BYTES);
            assert!(page.messages.iter().all(|m| m.body == body), "whole bodies");
            seen.extend(page.messages.iter().map(|m| m.id));
            if !advance_feed_cursor(&mut req, &page) {
                assert_eq!(page.caught_up, Some(true));
                break;
            }
            // Cut by bytes, not by the window.
            assert!(page.messages.len() < MAX_FEED_WINDOW as usize);
        }
        assert_eq!(pages, 2);
        assert_eq!(seen, ids, "every row once, in order");
    }
}
