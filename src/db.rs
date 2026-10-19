//! All SQL lives here, as plain functions over a `Connection`. Keep queries to
//! portable SQL so this module is the only thing that changes for another backend.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

/// The schema every migrating open advances to.
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_sessions.sql"),
    include_str!("../migrations/0003_agents_active_index.sql"),
    include_str!("../migrations/0004_agent_owners.sql"),
    include_str!("../migrations/0005_human_tokens.sql"),
    include_str!("../migrations/0006_group_archive.sql"),
    include_str!("../migrations/0007_group_description.sql"),
    include_str!("../migrations/0008_feed_indexes.sql"),
    include_str!("../migrations/0009_message_control.sql"),
    include_str!("../migrations/0010_group_writers.sql"),
    include_str!("../migrations/0011_activity.sql"),
    include_str!("../migrations/0012_timers.sql"),
    include_str!("../migrations/0013_human_dms.sql"),
    include_str!("../migrations/0014_activity_seen.sql"),
    include_str!("../migrations/0015_agent_effort.sql"),
    include_str!("../migrations/0016_agent_human_conversations.sql"),
    include_str!("../migrations/0017_auto_replies.sql"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Human,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Human => "human",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub kind: Kind,
    pub id: i64,
    pub name: String,
}

#[derive(Debug)]
pub struct AgentRow {
    pub id: i64,
    pub name: String,
    pub status: Option<String>,
    pub pwd: String,
    pub hostname: String,
    pub active_at: String,
    pub retired_at: Option<String>,
    pub owner: Option<String>,
    /// The newest message or activity event the agent sent; None when never.
    pub last_sent_at: Option<String>,
    /// The newest validated effort report (JSON) and when the server got it.
    pub effort: Option<String>,
    pub effort_reported_at: Option<String>,
    /// The newest direct message between the agent and any human; None when never.
    pub last_human_conversation_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GroupRow {
    pub name: String,
    pub description: Option<String>,
    pub members: Vec<String>,
    /// Members holding a write grant.
    pub writers: Vec<String>,
}

#[derive(Debug)]
pub struct HumanRow {
    pub name: String,
    pub active_at: String,
}

#[derive(Debug)]
pub struct MessageView {
    pub id: i64,
    pub created_at: String,
    pub from_kind: Kind,
    pub from_name: String,
    pub from_owner: Option<String>,
    pub reader_owner: Option<String>,
    pub to_group: Option<String>,
    /// Feed only: recipient of a direct message.
    pub to_name: Option<String>,
    /// Feed only: the direct recipient agent's human.
    pub to_owner: Option<String>,
    /// Feed only: when a direct message was read; always `None` for broadcasts.
    pub read_at: Option<String>,
    pub body: String,
    /// Inbox only: the control kind of a control message.
    pub control: Option<String>,
    /// Inbox only: the timer that sent it.
    pub timer_id: Option<i64>,
    /// Sent via AutoReply.
    pub auto_reply: bool,
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    enable_wal(&conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Consistent online snapshot through SQLite's backup API, without modifying or
/// migrating the source. Destination must be a new, private temporary file.
pub fn backup(source: &Path, destination: &Path) -> Result<()> {
    let source = Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening backup source {}", source.display()))?;
    source.busy_timeout(Duration::from_secs(5))?;
    // Pin a read snapshot so ongoing writes do not repeatedly restart the copy.
    source.execute_batch("BEGIN")?;
    let _: i64 = source.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    let mut target = Connection::open(destination)?;
    {
        let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if matches!(backup.step(256)?, rusqlite::backup::StepResult::Done) {
                break;
            }
            if Instant::now() >= deadline {
                bail!("backup did not finish within 60 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Publish a standalone database, without WAL or journal sidecars to copy.
    target.pragma_update(None, "journal_mode", "DELETE")?;
    target.close().map_err(|(_, error)| error)?;
    Ok(())
}

/// WAL is persistent, so this normally just reads the mode. Switching needs an
/// exclusive lock that the busy timeout does not cover, so retry while contended.
fn enable_wal(conn: &Connection) -> Result<()> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(err) if is_busy(&err) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err).context("enabling WAL"),
        }
    }
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn schema_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(v as usize)
}

/// Bring the schema up to date. Every command calls this on open; concurrent
/// callers serialize on the write lock and re-check the version under it.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    if schema_version(conn)? == MIGRATIONS.len() {
        return Ok(());
    }
    if !conn.is_autocommit() {
        bail!("BBS migrations require a connection without an active transaction");
    }
    // SQLite table rebuilds must disable foreign-key enforcement outside the
    // transaction. Validate the resulting references before commit and restore
    // the caller's setting even if a migration or commit fails.
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = schema_version(&tx)?;
        if current > MIGRATIONS.len() {
            bail!("BBS schema v{current} is newer than this binary supports");
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            tx.execute_batch(sql)
                .with_context(|| format!("applying migration {}", i + 1))?;
            tracing::info!(version = i + 1, "applied migration");
        }
        if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
            bail!("BBS migration would leave invalid foreign-key references");
        }
        tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", foreign_keys)?;
    result
}

// ---- humans ----------------------------------------------------------------

pub fn find_human(conn: &Connection, name: &str) -> Result<Option<Principal>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM humans WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok(Principal {
                    kind: Kind::Human,
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            },
        )
        .optional()?)
}

pub fn insert_human(conn: &Connection, name: &str, now: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO humans (name, created_at, active_at) VALUES (?1, ?2, ?2)",
        params![name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record activity, skipping the write unless `active_at` is older than
/// `stale_before`. A no-op commit leaves `PRAGMA data_version` alone, so
/// pollers only wake for real changes.
pub fn touch_human(conn: &Connection, id: i64, now: &str, stale_before: &str) -> Result<()> {
    conn.execute(
        "UPDATE humans SET active_at = ?1 WHERE id = ?2 AND active_at < ?3",
        params![now, id, stale_before],
    )?;
    Ok(())
}

pub fn human_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM humans ORDER BY id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

pub fn list_humans(conn: &Connection) -> Result<Vec<HumanRow>> {
    let mut stmt = conn.prepare("SELECT name, active_at FROM humans ORDER BY lower(name)")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HumanRow {
                name: r.get(0)?,
                active_at: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

// ---- agents ----------------------------------------------------------------

fn agent_principal(r: &rusqlite::Row) -> rusqlite::Result<Principal> {
    Ok(Principal {
        kind: Kind::Agent,
        id: r.get(0)?,
        name: r.get(1)?,
    })
}

pub fn agent_by_token(conn: &Connection, token: &str) -> Result<Option<Principal>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM agents WHERE token = ?1 AND retired_at IS NULL",
            params![token],
            agent_principal,
        )
        .optional()?)
}

pub fn live_agent_by_name(conn: &Connection, name: &str) -> Result<Option<Principal>> {
    let (name, owner) = agent_address(name);
    Ok(conn
        .query_row(
            "SELECT a.id, a.name FROM agents a
             LEFT JOIN agent_owners o ON o.agent_id = a.id
             LEFT JOIN humans h ON h.id = o.human_id
             WHERE lower(a.name) = lower(?1) AND a.retired_at IS NULL
               AND (?2 IS NULL OR lower(h.name) = lower(?2))",
            params![name, owner],
            agent_principal,
        )
        .optional()?)
}

fn agent_address(name: &str) -> (&str, Option<&str>) {
    match name.split_once('@') {
        Some((name, owner)) => (name, Some(owner)),
        None => (name, None),
    }
}

pub fn agent_is_live(conn: &Connection, id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE id = ?1 AND retired_at IS NULL",
        params![id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn human_by_token_hash(conn: &Connection, hash: &str) -> Result<Option<Principal>> {
    Ok(conn.query_row(
        "SELECT h.id, h.name FROM humans h JOIN human_tokens t ON t.human_id = h.id WHERE t.token_hash = ?1",
        [hash], |r| Ok(Principal { kind: Kind::Human, id: r.get(0)?, name: r.get(1)? })
    ).optional()?)
}

pub fn set_human_token(conn: &Connection, human_id: i64, hash: &str) -> Result<()> {
    conn.execute("INSERT INTO human_tokens (human_id, token_hash) VALUES (?1, ?2) ON CONFLICT(human_id) DO UPDATE SET token_hash = excluded.token_hash", params![human_id, hash])?;
    Ok(())
}

pub fn revoke_human_token(conn: &Connection, human_id: i64) -> Result<()> {
    conn.execute("DELETE FROM human_tokens WHERE human_id = ?1", [human_id])?;
    Ok(())
}

pub fn agent_owner_id(conn: &Connection, agent_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT human_id FROM agent_owners WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn agent_owner_enabled(conn: &Connection, agent_id: i64) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_owners o JOIN human_tokens t ON t.human_id = o.human_id WHERE o.agent_id = ?1)", [agent_id], |r| r.get(0))?)
}

pub fn session_binding(conn: &Connection, harness: &str, session_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT agent_id FROM sessions WHERE harness = ?1 AND session_id = ?2",
            params![harness, session_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn retired_agent_exists(conn: &Connection, name: &str) -> Result<bool> {
    let (name, owner) = agent_address(name);
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents a
         LEFT JOIN agent_owners o ON o.agent_id = a.id
         LEFT JOIN humans h ON h.id = o.human_id
         WHERE lower(a.name) = lower(?1) AND a.retired_at IS NOT NULL
           AND (?2 IS NULL OR lower(h.name) = lower(?2))",
        params![name, owner],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn token_exists(conn: &Connection, token: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE token = ?1",
        params![token],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Record activity, skipping the write unless `active_at` is older than
/// `stale_before`. A no-op commit leaves `PRAGMA data_version` alone, so
/// pollers only wake for real changes.
pub fn touch_agent(conn: &Connection, id: i64, now: &str, stale_before: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET active_at = ?1 WHERE id = ?2 AND active_at < ?3",
        params![now, id, stale_before],
    )?;
    Ok(())
}

pub fn stale_agent_ids(conn: &Connection, cutoff: &str) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT id FROM agents WHERE retired_at IS NULL AND active_at < ?1")?;
    let ids = stmt
        .query_map(params![cutoff], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Retire an agent: free its name, archive its unread mail, drop its memberships
/// and write grants.
pub fn retire_agent(conn: &Connection, id: i64, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET retired_at = ?1 WHERE id = ?2 AND retired_at IS NULL",
        params![now, id],
    )?;
    conn.execute(
        "UPDATE deliveries SET archived_at = ?1
         WHERE recipient_kind = 'agent' AND recipient_id = ?2 AND archived_at IS NULL",
        params![now, id],
    )?;
    conn.execute("DELETE FROM group_members WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM group_writers WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM activity WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM timers WHERE target_id = ?1", params![id])?;
    conn.execute("DELETE FROM activity_seen WHERE agent_id = ?1", params![id])?;
    conn.execute(
        "UPDATE agents SET effort = NULL, effort_reported_at = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Store an agent's validated effort report JSON and when it arrived.
pub fn set_effort(conn: &Connection, id: i64, report: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET effort = ?2, effort_reported_at = ?3 WHERE id = ?1",
        params![id, report, now],
    )?;
    Ok(())
}

/// The body of message `id` when it is a set-effort control delivered to
/// agent `agent`.
pub fn effort_control_body(conn: &Connection, agent: i64, id: i64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT m.body FROM deliveries d JOIN messages m ON m.id = d.message_id
         WHERE d.recipient_kind = 'agent' AND d.recipient_id = ?1
           AND m.id = ?2 AND m.control = 'set-effort'",
        params![agent, id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Lowercased names currently held by live agents or humans.
pub fn taken_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT lower(name) FROM agents WHERE retired_at IS NULL
         UNION SELECT lower(name) FROM humans",
    )?;
    let names = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(names)
}

/// Lowercased agent name -> most recent registration time under that name.
pub fn name_last_used(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT lower(name), MAX(created_at) FROM agents GROUP BY lower(name)")?;
    let map = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(map)
}

pub struct NewAgent<'a> {
    pub name: &'a str,
    pub token: &'a str,
    pub owner_id: i64,
    pub status: Option<&'a str>,
    pub pwd: &'a str,
    pub hostname: &'a str,
    pub now: &'a str,
}

pub fn insert_agent(conn: &Connection, a: &NewAgent) -> Result<i64> {
    let status_at = a.status.map(|_| a.now);
    conn.execute(
        "INSERT INTO agents (name, token, status, status_at, pwd, hostname, created_at, active_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![
            a.name, a.token, a.status, status_at, a.pwd, a.hostname, a.now
        ],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO agent_owners (agent_id, human_id) VALUES (?1, ?2)",
        params![id, a.owner_id],
    )?;
    Ok(id)
}

pub fn set_status(conn: &Connection, id: i64, status: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET status = ?1, status_at = ?2 WHERE id = ?3",
        params![status, now, id],
    )?;
    Ok(())
}

pub fn list_agents(conn: &Connection, include_retired: bool) -> Result<Vec<AgentRow>> {
    // last_sent_at is the newest server-stamped message or activity the agent
    // sent. Ids grow with server time, so each source's newest row comes from
    // one bounded index walk (messages_sender, activity_agent), not a scan of
    // the sender's history.
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.status, a.pwd, a.hostname, a.active_at, a.retired_at, h.name,
                a.effort, a.effort_reported_at,
                (SELECT MAX(created_at) FROM (
                   SELECT created_at FROM (
                     SELECT created_at FROM messages
                     WHERE from_kind = 'agent' AND from_id = a.id
                     ORDER BY id DESC LIMIT 1)
                   UNION ALL
                   SELECT created_at FROM (
                     SELECT created_at FROM activity
                     WHERE agent_id = a.id
                     ORDER BY id DESC LIMIT 1))),
                (SELECT max(last_at) FROM agent_human_conversations p WHERE p.agent_id = a.id)
         FROM agents a
         LEFT JOIN agent_owners o ON o.agent_id = a.id
         LEFT JOIN humans h ON h.id = o.human_id
         WHERE a.retired_at IS NULL OR ?1
         ORDER BY a.retired_at IS NOT NULL, lower(a.name), a.id",
    )?;
    let rows = stmt
        .query_map(params![include_retired], |r| {
            Ok(AgentRow {
                id: r.get(0)?,
                name: r.get(1)?,
                status: r.get(2)?,
                pwd: r.get(3)?,
                hostname: r.get(4)?,
                active_at: r.get(5)?,
                retired_at: r.get(6)?,
                owner: r.get(7)?,
                effort: r.get(8)?,
                effort_reported_at: r.get(9)?,
                last_sent_at: r.get(10)?,
                last_human_conversation_at: r.get(11)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

// ---- groups ----------------------------------------------------------------

pub fn group_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM bbs_groups WHERE name = ?1 AND archived_at IS NULL",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn insert_group(
    conn: &Connection,
    name: &str,
    description: Option<&str>,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO bbs_groups (name, description, created_at) VALUES (?1, ?2, ?3)",
        params![name, description, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Set or, with `None`, clear an active group's description.
pub fn set_group_description(
    conn: &Connection,
    group_id: i64,
    description: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE bbs_groups SET description = ?1 WHERE id = ?2 AND archived_at IS NULL",
        params![description, group_id],
    )?;
    Ok(())
}

pub fn archive_group(conn: &Connection, group_id: i64, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE bbs_groups SET archived_at = ?1 WHERE id = ?2 AND archived_at IS NULL",
        params![now, group_id],
    )?;
    Ok(())
}

/// Returns false if the agent was already a member.
pub fn join_group(conn: &Connection, group_id: i64, agent_id: i64, now: &str) -> Result<bool> {
    let n = conn.execute(
        "INSERT INTO group_members (group_id, agent_id, joined_at) VALUES (?1, ?2, ?3)
         ON CONFLICT DO NOTHING",
        params![group_id, agent_id, now],
    )?;
    Ok(n > 0)
}

/// Returns false if the agent was not a member.
pub fn leave_group(conn: &Connection, group_id: i64, agent_id: i64) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM group_members WHERE group_id = ?1 AND agent_id = ?2",
        params![group_id, agent_id],
    )?;
    Ok(n > 0)
}

pub fn is_member(conn: &Connection, group_id: i64, agent_id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM group_members m JOIN bbs_groups g ON g.id = m.group_id
         WHERE m.group_id = ?1 AND m.agent_id = ?2 AND g.archived_at IS NULL",
        params![group_id, agent_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn is_writer(conn: &Connection, group_id: i64, agent_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM group_writers WHERE group_id = ?1 AND agent_id = ?2)",
        params![group_id, agent_id],
        |r| r.get(0),
    )?)
}

/// Grant or revoke write; returns false if nothing changed.
pub fn set_writer(
    conn: &Connection,
    group_id: i64,
    agent_id: i64,
    grant: bool,
    now: &str,
) -> Result<bool> {
    let n = if grant {
        conn.execute(
            "INSERT OR IGNORE INTO group_writers (group_id, agent_id, granted_at)
             VALUES (?1, ?2, ?3)",
            params![group_id, agent_id, now],
        )?
    } else {
        conn.execute(
            "DELETE FROM group_writers WHERE group_id = ?1 AND agent_id = ?2",
            params![group_id, agent_id],
        )?
    };
    Ok(n > 0)
}

/// Names of the group's live members that hold a write grant.
pub fn group_writers(conn: &Connection, group_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT a.name FROM group_members m JOIN agents a ON a.id = m.agent_id
           JOIN group_writers w ON w.group_id = m.group_id AND w.agent_id = m.agent_id
          WHERE m.group_id = ?1 AND a.retired_at IS NULL
          ORDER BY lower(a.name)",
    )?;
    let names = stmt
        .query_map(params![group_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(names)
}

pub fn share_group(conn: &Connection, a: i64, b: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM group_members x
         JOIN group_members y ON x.group_id = y.group_id
         JOIN bbs_groups g ON g.id = x.group_id
         WHERE x.agent_id = ?1 AND y.agent_id = ?2 AND g.archived_at IS NULL",
        params![a, b],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn group_member_ids(conn: &Connection, group_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT m.agent_id FROM group_members m JOIN agents a ON a.id = m.agent_id
         JOIN bbs_groups g ON g.id = m.group_id
         WHERE m.group_id = ?1 AND a.retired_at IS NULL AND g.archived_at IS NULL
         ORDER BY m.agent_id",
    )?;
    let ids = stmt
        .query_map(params![group_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// (group id, group name) for every group `agent_id` belongs to.
pub fn member_groups(conn: &Connection, agent_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name FROM group_members m JOIN bbs_groups g ON g.id = m.group_id
         WHERE m.agent_id = ?1 AND g.archived_at IS NULL ORDER BY g.name",
    )?;
    let groups = stmt
        .query_map(params![agent_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(groups)
}

/// Every active group with its description and member names, or only those
/// `agent_id` belongs to.
pub fn list_groups(conn: &Connection, agent_id: Option<i64>) -> Result<Vec<GroupRow>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name, g.description, a.name, w.group_id IS NOT NULL
         FROM bbs_groups g
         LEFT JOIN group_members m ON m.group_id = g.id
         LEFT JOIN agents a ON a.id = m.agent_id
         LEFT JOIN group_writers w ON w.group_id = g.id AND w.agent_id = a.id
         WHERE g.archived_at IS NULL
         AND (?1 IS NULL OR g.id IN (SELECT group_id FROM group_members WHERE agent_id = ?1))
         ORDER BY g.name, lower(a.name)",
    )?;
    let mut groups: Vec<(i64, GroupRow)> = Vec::new();
    let rows = stmt.query_map(params![agent_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })?;
    for row in rows {
        let (id, name, description, member, writer) = row?;
        if groups.last().is_none_or(|(g, _)| *g != id) {
            let group = GroupRow {
                name,
                description,
                members: Vec::new(),
                writers: Vec::new(),
            };
            groups.push((id, group));
        }
        if let Some(member) = member {
            let group = &mut groups.last_mut().unwrap().1;
            if writer {
                group.writers.push(member.clone());
            }
            group.members.push(member);
        }
    }
    Ok(groups.into_iter().map(|(_, group)| group).collect())
}

/// Agent id -> names of the groups it belongs to.
pub fn agent_groups(conn: &Connection) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT m.agent_id, g.name FROM group_members m
         JOIN bbs_groups g ON g.id = m.group_id
         WHERE g.archived_at IS NULL ORDER BY g.name",
    )?;
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, group) = row?;
        map.entry(id).or_default().push(group);
    }
    Ok(map)
}

// ---- messages --------------------------------------------------------------

pub fn insert_message(
    conn: &Connection,
    from: &Principal,
    to_group_id: Option<i64>,
    body: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (from_kind, from_id, to_group_id, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from.kind.as_str(), from.id, to_group_id, body, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Mark message `id` as a control message of kind `control`.
pub fn set_message_control(conn: &Connection, id: i64, control: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET control = ?2 WHERE id = ?1",
        params![id, control],
    )?;
    Ok(())
}

/// Replace a live agent's token and unbind its harness sessions, which would
/// otherwise hand the new token to whoever held the old one.
pub fn set_agent_token(conn: &Connection, agent_id: i64, token: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET token = ?2 WHERE id = ?1 AND retired_at IS NULL",
        params![agent_id, token],
    )?;
    conn.execute("DELETE FROM sessions WHERE agent_id = ?1", [agent_id])?;
    Ok(())
}

pub fn insert_delivery(conn: &Connection, message_id: i64, kind: Kind, id: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO deliveries (message_id, recipient_kind, recipient_id, direct)
         SELECT id, ?2, ?3, to_group_id IS NULL FROM messages WHERE id = ?1",
        params![message_id, kind.as_str(), id],
    )?;
    Ok(())
}

/// Mark message `id` as an automatic owner reply.
pub fn set_message_auto_reply(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE messages SET auto_reply = 1 WHERE id = ?1", [id])?;
    Ok(())
}

/// An accepted auto-reply for this (registration, key), if any: its message
/// id, the body as stored, and the human it was delivered to.
pub fn find_auto_reply(
    conn: &Connection,
    agent_id: i64,
    key: &str,
) -> Result<Option<(i64, String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT ar.message_id, m.body, d.recipient_id
             FROM auto_replies ar
             JOIN messages m ON m.id = ar.message_id
             JOIN deliveries d ON d.message_id = ar.message_id AND d.recipient_kind = 'human'
             WHERE ar.agent_id = ?1 AND ar.key = ?2",
            params![agent_id, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// Record an accepted auto-reply. The primary key rejects a conflicting
/// duplicate; callers treat that as a replay after re-reading.
pub fn insert_auto_reply(
    conn: &Connection,
    agent_id: i64,
    key: &str,
    message_id: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO auto_replies (agent_id, key, message_id) VALUES (?1, ?2, ?3)",
        params![agent_id, key, message_id],
    )?;
    Ok(())
}

/// The owning human's name, when the agent has a registered owner.
pub fn agent_owner_name(conn: &Connection, agent_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT h.name FROM agent_owners o JOIN humans h ON h.id = o.human_id
             WHERE o.agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Record a direct message between `agent` and `human`, in either direction,
/// at `at`. Monotonic: an older time never replaces a newer one.
pub fn record_human_conversation(
    conn: &Connection,
    agent: i64,
    human: i64,
    at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_human_conversations (agent_id, human_id, last_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (agent_id, human_id) DO UPDATE SET last_at = excluded.last_at
         WHERE excluded.last_at > agent_human_conversations.last_at",
        params![agent, human, at],
    )?;
    Ok(())
}

const INBOX_SELECT: &str = "
    SELECT m.id, m.created_at, m.from_kind,
           CASE m.from_kind WHEN 'agent' THEN a.name ELSE h.name END,
           g.name, m.body, owner.name, reader_owner.name, m.control, m.timer_id, m.auto_reply
    FROM deliveries d
    JOIN messages m ON m.id = d.message_id
    LEFT JOIN agents a ON m.from_kind = 'agent' AND a.id = m.from_id
    LEFT JOIN humans h ON m.from_kind = 'human' AND h.id = m.from_id
    LEFT JOIN agent_owners sender ON sender.agent_id = a.id
    LEFT JOIN humans owner ON owner.id = sender.human_id
    LEFT JOIN agent_owners reader ON d.recipient_kind = 'agent' AND reader.agent_id = d.recipient_id
    LEFT JOIN humans reader_owner ON reader_owner.id = reader.human_id
    LEFT JOIN bbs_groups g ON g.id = m.to_group_id
    WHERE d.recipient_kind = ?1 AND d.recipient_id = ?2";

fn message_view(r: &rusqlite::Row) -> rusqlite::Result<MessageView> {
    let from_kind: String = r.get(2)?;
    Ok(MessageView {
        id: r.get(0)?,
        created_at: r.get(1)?,
        from_kind: if from_kind == "human" {
            Kind::Human
        } else {
            Kind::Agent
        },
        from_name: r.get(3)?,
        from_owner: r.get(6)?,
        reader_owner: r.get(7)?,
        to_group: r.get(4)?,
        to_name: None,
        to_owner: None,
        read_at: None,
        body: r.get(5)?,
        control: r.get(8)?,
        timer_id: r.get(9)?,
        auto_reply: r.get::<_, Option<i64>>(10)?.is_some_and(|v| v != 0),
    })
}

/// Mark read `me`'s unread mail from one sender; returns how many.
pub fn mark_read_from(
    conn: &Connection,
    me: &Principal,
    from: &Principal,
    now: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE deliveries SET archived_at = ?1
         WHERE recipient_kind = ?2 AND recipient_id = ?3 AND archived_at IS NULL
           AND message_id IN (SELECT id FROM messages WHERE from_kind = ?4 AND from_id = ?5)",
        params![now, me.kind.as_str(), me.id, from.kind.as_str(), from.id],
    )?)
}

/// What a human has yet to look at from one agent, and when it last sent.
#[derive(Debug, PartialEq)]
pub struct AgentSummary {
    pub agent: String,
    pub unread_messages: u32,
    pub oldest_unread_message: Option<String>,
    pub unseen_stream: u32,
    pub oldest_unseen_stream: Option<String>,
    /// The newest message or activity event the agent sent; None when never.
    pub last_sent_at: Option<String>,
    /// The newest validated effort report (JSON) and when the server got it.
    pub effort: Option<String>,
    pub effort_reported_at: Option<String>,
    /// The newest direct message between the agent and any human; None when never.
    pub last_human_conversation_at: Option<String>,
}

/// Unread mail through the human's inbox index; unseen activity per live
/// agent through `activity_agent`, from the human's cursor, so neither walks
/// the whole activity table. Last-sent times come from `messages_sender` and
/// `activity_agent` the same way.
const AGENT_SUMMARIES: &str = "
    WITH unread AS (
        SELECT m.from_id AS agent_id, count(*) AS n, min(m.created_at) AS oldest
        FROM deliveries d JOIN messages m ON m.id = d.message_id
        WHERE d.recipient_kind = 'human' AND d.recipient_id = ?1
          AND d.archived_at IS NULL AND d.direct = 1 AND m.from_kind = 'agent'
        GROUP BY m.from_id),
    summaries AS MATERIALIZED (
        SELECT g.name, coalesce(u.n, 0) AS unread, u.oldest AS unread_oldest,
               g.effort, g.effort_reported_at,
               (SELECT count(*) FROM activity act
                WHERE act.agent_id = g.id AND act.id > coalesce(s.seen_id, 0)) AS unseen,
               -- Ids grow with arrival, so the first unseen is the oldest.
               (SELECT act.created_at FROM activity act
                WHERE act.agent_id = g.id AND act.id > coalesce(s.seen_id, 0)
                ORDER BY act.id LIMIT 1) AS unseen_oldest,
               (SELECT MAX(created_at) FROM (
                   SELECT created_at FROM (
                     SELECT created_at FROM messages
                     WHERE from_kind = 'agent' AND from_id = g.id
                     ORDER BY id DESC LIMIT 1)
                   UNION ALL
                   SELECT created_at FROM (
                     SELECT created_at FROM activity
                     WHERE agent_id = g.id
                     ORDER BY id DESC LIMIT 1))) AS last_sent,
               (SELECT max(p.last_at) FROM agent_human_conversations p
                WHERE p.agent_id = g.id) AS human_conversation
        FROM agents g
        LEFT JOIN activity_seen s ON s.human_id = ?1 AND s.agent_id = g.id
        LEFT JOIN unread u ON u.agent_id = g.id
        WHERE g.retired_at IS NULL)
    SELECT name, unread, unread_oldest, unseen, unseen_oldest, last_sent, effort,
           effort_reported_at, human_conversation FROM summaries
    ORDER BY lower(name)";

/// For each live agent, the direct messages it sent `human` that are unread,
/// the shared activity `human` has not seen, and when it last sent anything.
/// Broadcasts don't count as unread: humans receive every one.
pub fn agent_summaries(conn: &Connection, human: i64) -> Result<Vec<AgentSummary>> {
    let mut stmt = conn.prepare_cached(AGENT_SUMMARIES)?;
    let rows = stmt
        .query_map([human], |r| {
            Ok(AgentSummary {
                agent: r.get(0)?,
                unread_messages: r.get(1)?,
                oldest_unread_message: r.get(2)?,
                unseen_stream: r.get(3)?,
                oldest_unseen_stream: r.get(4)?,
                last_sent_at: r.get(5)?,
                effort: r.get(6)?,
                effort_reported_at: r.get(7)?,
                last_human_conversation_at: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Record that `human` has seen `agent`'s activity through `through_id`, kept
/// to the agent's newest activity so later records stay unseen. A cursor
/// never moves back; returns whether it moved.
pub fn mark_seen(conn: &Connection, human: i64, agent: i64, through_id: i64) -> Result<bool> {
    let moved = conn.execute(
        "INSERT INTO activity_seen (human_id, agent_id, seen_id)
         SELECT ?1, ?2, min(?3, max(id)) FROM activity WHERE agent_id = ?2
         HAVING max(id) IS NOT NULL
         ON CONFLICT (human_id, agent_id) DO UPDATE SET seen_id = excluded.seen_id
             WHERE excluded.seen_id > activity_seen.seen_id",
        params![human, agent, through_id],
    )?;
    Ok(moved > 0)
}

pub fn has_unread(conn: &Connection, me: &Principal) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM deliveries
         WHERE recipient_kind = ?1 AND recipient_id = ?2 AND archived_at IS NULL)",
        params![me.kind.as_str(), me.id],
        |r| r.get(0),
    )?;
    Ok(n != 0)
}

/// Fetch unread messages oldest-first and mark them archived.
pub fn take_unread(
    conn: &Connection,
    me: &Principal,
    limit: Option<u32>,
    now: &str,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{INBOX_SELECT} AND d.archived_at IS NULL
         ORDER BY m.created_at, m.id LIMIT ?3"
    );
    let limit = limit.map_or(i64::MAX, i64::from);
    let msgs: Vec<MessageView> = conn
        .prepare(&sql)?
        .query_map(params![me.kind.as_str(), me.id, limit], message_view)?
        .collect::<Result<_, _>>()?;
    let mut archive = conn.prepare(
        "UPDATE deliveries SET archived_at = ?1
         WHERE message_id = ?2 AND recipient_kind = ?3 AND recipient_id = ?4",
    )?;
    for m in &msgs {
        archive.execute(params![now, m.id, me.kind.as_str(), me.id])?;
    }
    Ok(msgs)
}

pub fn archived_since(
    conn: &Connection,
    me: &Principal,
    since: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{INBOX_SELECT} AND d.archived_at IS NOT NULL AND m.control IS NULL
         AND (?3 IS NULL OR m.created_at >= ?3)
         ORDER BY m.created_at, m.id LIMIT ?4"
    );
    let limit = limit.map_or(i64::MAX, i64::from);
    let msgs = conn
        .prepare(&sql)?
        .query_map(params![me.kind.as_str(), me.id, since, limit], message_view)?
        .collect::<Result<_, _>>()?;
    Ok(msgs)
}

// ---- harness sessions ------------------------------------------------------

/// Bind a harness session to an agent, returning the agent it was previously bound to.
pub fn bind_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    agent_id: i64,
    now: &str,
) -> Result<Option<i64>> {
    let previous: Option<i64> = conn
        .query_row(
            "SELECT agent_id FROM sessions WHERE harness = ?1 AND session_id = ?2",
            params![harness, session_id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO sessions (harness, session_id, agent_id, bound_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (harness, session_id) DO UPDATE SET agent_id = ?3, bound_at = ?4",
        params![harness, session_id, agent_id, now],
    )?;
    Ok(previous.filter(|&id| id != agent_id))
}

/// The live agent bound to a harness session, with its token.
pub fn session_agent(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> Result<Option<(Principal, String)>> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.name, a.token FROM sessions s JOIN agents a ON a.id = s.agent_id
             WHERE s.harness = ?1 AND s.session_id = ?2 AND a.retired_at IS NULL",
            params![harness, session_id],
            |r| Ok((agent_principal(r)?, r.get(2)?)),
        )
        .optional()?)
}

// ---- feed (bbs view) -------------------------------------------------------

/// Feed filters, with names already resolved to ids; each set field narrows
/// the result. Only the filters present reach the SQL, so each can use an index.
#[derive(Debug, Default)]
pub struct FeedFilter<'a> {
    /// Broadcasts to any of these groups (every group, active or archived, of a name).
    pub group_ids: Option<Vec<i64>>,
    /// Sent by any of these principals (every human and agent of a name).
    pub senders: Option<Vec<(Kind, i64)>>,
    /// Sent by this human id or by an agent it owns.
    pub from_human: Option<i64>,
    /// Sent by any of these principals, or sent directly to one of them.
    pub with: Option<Vec<(Kind, i64)>>,
    /// `created_at >= since`.
    pub since: Option<&'a str>,
    /// `created_at < until`.
    pub until: Option<&'a str>,
    /// This human id's scope: sent by it or its agents, delivered to one of its
    /// agents (so group mail counts by membership at send time), or a direct
    /// message to the human itself.
    pub scope: Option<i64>,
    /// Cursor mode: the oldest matching messages with a greater id, instead of
    /// the most recent window.
    pub after_id: Option<i64>,
}

/// Ids of every group, active or archived, with this name.
pub fn group_ids_named(conn: &Connection, name: &str) -> Result<Vec<i64>> {
    Ok(conn
        .prepare("SELECT id FROM bbs_groups WHERE name = ?1")?
        .query_map(params![name], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Every human and agent, live or retired, with this name, ignoring case.
pub fn principals_named(conn: &Connection, name: &str) -> Result<Vec<(Kind, i64)>> {
    let mut out = Vec::new();
    for (kind, sql) in [
        (
            Kind::Human,
            "SELECT id FROM humans WHERE lower(name) = lower(?1)",
        ),
        (
            Kind::Agent,
            "SELECT id FROM agents WHERE lower(name) = lower(?1)",
        ),
    ] {
        let ids = conn
            .prepare(sql)?
            .query_map(params![name], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        out.extend(ids.into_iter().map(|id| (kind, id)));
    }
    Ok(out)
}

/// Every agent, live or retired, this human owns.
fn owned_agents(conn: &Connection, human: i64) -> Result<Vec<i64>> {
    Ok(conn
        .prepare_cached("SELECT agent_id FROM agent_owners WHERE human_id = ?1")?
        .query_map(params![human], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

type Args = Vec<rusqlite::types::Value>;

/// One way a message can match an any-of filter, walked through its own index
/// in id order so a dense match stops after `limit` rows and a sparse one never
/// scans unrelated messages.
#[derive(Debug, Clone, Copy)]
enum Term {
    /// Sent by this principal.
    Sent(Kind, i64),
    /// Delivered to this agent.
    DeliveredTo(i64),
    /// A direct message to this human.
    DirectTo(i64),
    /// A direct message to this agent.
    DirectToAgent(i64),
    /// A broadcast to this group.
    ToGroup(i64),
}

/// Which any-of filter is expanded into terms; the others stay predicates.
#[derive(Clone, Copy, PartialEq)]
enum Expanded {
    None,
    Senders,
    FromHuman,
    Scope,
    Groups,
    With,
}

const OWNED_AGENTS: &str = "SELECT agent_id FROM agent_owners WHERE human_id = ?";

/// `AND`-joined predicates for every filter except the expanded one.
fn feed_predicates(filter: &FeedFilter, expanded: Expanded) -> (String, Args) {
    use rusqlite::types::Value;
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Args = Vec::new();
    let list = |args: &mut Args, ids: &mut dyn Iterator<Item = i64>| -> String {
        let n = args.len();
        args.extend(ids.map(Value::Integer));
        vec!["?"; args.len() - n].join(", ")
    };
    if let Some(ids) = filter
        .group_ids
        .as_ref()
        .filter(|_| expanded != Expanded::Groups)
    {
        clauses.push(format!(
            "m.to_group_id IN ({})",
            list(&mut args, &mut ids.iter().copied())
        ));
    }
    if let Some(senders) = filter
        .senders
        .as_ref()
        .filter(|_| expanded != Expanded::Senders)
    {
        let mut any = vec!["0".to_string()];
        for kind in [Kind::Human, Kind::Agent] {
            let mut ids = senders
                .iter()
                .filter(|s| s.0 == kind)
                .map(|s| s.1)
                .peekable();
            if ids.peek().is_some() {
                any.push(format!(
                    "(m.from_kind = '{}' AND m.from_id IN ({}))",
                    kind.as_str(),
                    list(&mut args, &mut ids)
                ));
            }
        }
        clauses.push(format!("({})", any.join(" OR ")));
    }
    if let Some(human) = filter
        .from_human
        .filter(|_| expanded != Expanded::FromHuman)
    {
        clauses.push(format!(
            "((m.from_kind = 'human' AND m.from_id = ?)
              OR (m.from_kind = 'agent' AND m.from_id IN ({OWNED_AGENTS})))"
        ));
        args.extend([human; 2].map(Value::Integer));
    }
    if let Some(human) = filter.scope.filter(|_| expanded != Expanded::Scope) {
        clauses.push(format!(
            "((m.from_kind = 'human' AND m.from_id = ?)
              OR (m.from_kind = 'agent' AND m.from_id IN ({OWNED_AGENTS}))
              OR m.id IN (SELECT message_id FROM deliveries
                          WHERE recipient_kind = 'agent' AND recipient_id IN ({OWNED_AGENTS}))
              OR (m.to_group_id IS NULL AND m.from_kind = 'human'
                  AND m.id IN (SELECT message_id FROM deliveries
                               WHERE recipient_kind = 'human' AND recipient_id = ?)))"
        ));
        args.extend([human; 4].map(Value::Integer));
    }
    if let Some(since) = filter.since {
        clauses.push("m.created_at >= ?".into());
        args.push(Value::Text(since.into()));
    }
    if let Some(until) = filter.until {
        clauses.push("m.created_at < ?".into());
        args.push(Value::Text(until.into()));
    }
    if let Some(after) = filter.after_id {
        clauses.push("m.id > ?".into());
        args.push(Value::Integer(after));
    }
    // Control messages (cancel, set-effort) act through the inbox and are
    // never ordinary feed or history content.
    clauses.push("m.control IS NULL".into());
    let mut sql = String::new();
    for clause in clauses {
        sql.push_str(" AND ");
        sql.push_str(&clause);
    }
    (sql, args)
}

fn direction(filter: &FeedFilter) -> &'static str {
    if filter.after_id.is_some() {
        "ASC"
    } else {
        "DESC"
    }
}

/// The page of ids for a filter with no any-of term: one indexed walk.
fn page_sql(limit: u32, filter: &FeedFilter) -> (String, Args) {
    let (predicates, mut args) = feed_predicates(filter, Expanded::None);
    let dir = direction(filter);
    // A window bounded only by time walks the time index from its newest end
    // and stops after `limit` rows; ordering by id instead would scan every
    // newer message outside the range. Ids and send times rise together (both
    // are assigned under the write lock), so the window is the same.
    let time_only = (filter.since.is_some() || filter.until.is_some())
        && filter.group_ids.is_none()
        && filter.after_id.is_none();
    let order = if time_only {
        format!("m.created_at {dir}, m.id {dir}")
    } else {
        format!("m.id {dir}")
    };
    args.push(rusqlite::types::Value::Integer(limit.into()));
    (
        format!("SELECT m.id FROM messages m WHERE 1{predicates} ORDER BY {order} LIMIT ?"),
        args,
    )
}

/// The first `limit` ids, in feed order, matching one term and the predicates.
fn term_sql(term: Term, limit: u32, filter: &FeedFilter, expanded: Expanded) -> (String, Args) {
    use rusqlite::types::Value;
    let (predicates, rest) = feed_predicates(filter, expanded);
    let dir = direction(filter);
    let (sql, mut args) = match term {
        Term::Sent(kind, id) => (
            format!(
                "SELECT m.id FROM messages m WHERE m.from_kind = ? AND m.from_id = ?{predicates}
                 ORDER BY m.id {dir} LIMIT ?"
            ),
            vec![Value::Text(kind.as_str().into()), Value::Integer(id)],
        ),
        Term::DeliveredTo(agent) => (
            format!(
                "SELECT m.id FROM deliveries d JOIN messages m ON m.id = d.message_id
                 WHERE d.recipient_kind = 'agent' AND d.recipient_id = ?{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?"
            ),
            vec![Value::Integer(agent)],
        ),
        Term::DirectToAgent(agent) => (
            format!(
                "SELECT m.id FROM deliveries d JOIN messages m ON m.id = d.message_id
                 WHERE d.recipient_kind = 'agent' AND d.recipient_id = ?
                   AND m.to_group_id IS NULL{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?"
            ),
            vec![Value::Integer(agent)],
        ),
        Term::ToGroup(group) => (
            format!(
                "SELECT m.id FROM messages m WHERE m.to_group_id = ?{predicates}
                 ORDER BY m.id {dir} LIMIT ?"
            ),
            vec![Value::Integer(group)],
        ),
        Term::DirectTo(human) => (
            format!(
                "SELECT m.id FROM deliveries d INDEXED BY deliveries_human_dm
                 JOIN messages m ON m.id = d.message_id
                 WHERE d.recipient_kind = 'human' AND d.direct = 1
                   AND d.recipient_id = ?{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?"
            ),
            vec![Value::Integer(human)],
        ),
    };
    args.extend(rest);
    args.push(Value::Integer(limit.into()));
    (sql, args)
}

/// The ids of the page: the first `limit` matches in feed order. An any-of
/// filter is answered as the union of each term's own first `limit`, which
/// holds the overall first `limit`.
fn page_ids(conn: &Connection, limit: u32, filter: &FeedFilter) -> Result<Vec<i64>> {
    let (expanded, terms) = if let Some(with) = &filter.with {
        let mut terms = Vec::new();
        for &(kind, id) in with {
            terms.push(Term::Sent(kind, id));
            terms.push(match kind {
                Kind::Human => Term::DirectTo(id),
                Kind::Agent => Term::DirectToAgent(id),
            });
        }
        (Expanded::With, terms)
    } else if let Some(senders) = &filter.senders {
        let terms = senders.iter().map(|&(k, id)| Term::Sent(k, id)).collect();
        (Expanded::Senders, terms)
    } else if let Some(human) = filter.from_human {
        let mut terms = vec![Term::Sent(Kind::Human, human)];
        let agents = owned_agents(conn, human)?;
        terms.extend(agents.into_iter().map(|a| Term::Sent(Kind::Agent, a)));
        (Expanded::FromHuman, terms)
    } else if let Some(human) = filter.scope {
        let mut terms = vec![Term::Sent(Kind::Human, human), Term::DirectTo(human)];
        for agent in owned_agents(conn, human)? {
            terms.extend([Term::Sent(Kind::Agent, agent), Term::DeliveredTo(agent)]);
        }
        (Expanded::Scope, terms)
    } else if let Some(ids) = filter.group_ids.as_ref().filter(|ids| ids.len() > 1) {
        // A reused group name: one walk per incarnation avoids sorting them all.
        (
            Expanded::Groups,
            ids.iter().map(|&g| Term::ToGroup(g)).collect(),
        )
    } else {
        let (sql, args) = page_sql(limit, filter);
        return Ok(conn
            .prepare_cached(&sql)?
            .query_map(rusqlite::params_from_iter(args), |r| r.get(0))?
            .collect::<Result<_, _>>()?);
    };
    let mut ids = std::collections::BTreeSet::new();
    for term in terms {
        let (sql, args) = term_sql(term, limit, filter, expanded);
        let mut stmt = conn.prepare_cached(&sql)?;
        for id in stmt.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, i64>(0))? {
            ids.insert(id?);
        }
    }
    let limit = limit as usize;
    Ok(if filter.after_id.is_some() {
        ids.into_iter().take(limit).collect()
    } else {
        ids.into_iter().rev().take(limit).collect()
    })
}

/// Messages matching `filter`, oldest first, at most `limit`: the most recent
/// ones, or in cursor mode (`after_id`) the oldest after the cursor. Each
/// broadcast appears once, regardless of how many deliveries it fanned out to.
pub fn feed_messages(
    conn: &Connection,
    limit: u32,
    filter: &FeedFilter,
) -> Result<Vec<MessageView>> {
    let ids = page_ids(conn, limit, filter)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT m.id, m.created_at, m.from_kind, COALESCE(fa.name, fh.name), g.name,
                COALESCE(ta.name, th.name), d.archived_at, m.body, owner.name, to_owner.name,
                m.auto_reply
         FROM messages m
         LEFT JOIN agents fa ON m.from_kind = 'agent' AND fa.id = m.from_id
         LEFT JOIN humans fh ON m.from_kind = 'human' AND fh.id = m.from_id
         LEFT JOIN agent_owners sender ON sender.agent_id = fa.id
         LEFT JOIN humans owner ON owner.id = sender.human_id
         LEFT JOIN bbs_groups g ON g.id = m.to_group_id
         LEFT JOIN deliveries d ON m.to_group_id IS NULL AND d.message_id = m.id
         LEFT JOIN agents ta ON d.recipient_kind = 'agent' AND ta.id = d.recipient_id
         LEFT JOIN humans th ON d.recipient_kind = 'human' AND th.id = d.recipient_id
         LEFT JOIN agent_owners recipient ON recipient.agent_id = ta.id
         LEFT JOIN humans to_owner ON to_owner.id = recipient.human_id
         WHERE m.id IN ({})
         ORDER BY m.id",
        vec!["?"; ids.len()].join(", ")
    );
    let rows = conn
        .prepare(&sql)?
        .query_map(rusqlite::params_from_iter(ids), |r| {
            let from_kind: String = r.get(2)?;
            Ok(MessageView {
                id: r.get(0)?,
                created_at: r.get(1)?,
                from_kind: if from_kind == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                from_name: r.get(3)?,
                from_owner: r.get(8)?,
                reader_owner: None,
                to_group: r.get(4)?,
                to_name: r.get(5)?,
                to_owner: r.get(9)?,
                read_at: r.get(6)?,
                body: r.get(7)?,
                control: None,
                timer_id: None,
                auto_reply: r.get::<_, Option<i64>>(10)?.is_some_and(|v| v != 0),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

// ---- activity (agent-activity.v1) -----------------------------------------

/// Events kept per agent; older ones are trimmed as new ones arrive.
pub const ACTIVITY_PER_AGENT: i64 = 2000;

pub struct ActivityRow {
    pub id: i64,
    pub created_at: String,
    pub agent: String,
    pub owner: Option<String>,
    pub record: String,
}

/// Store `events` (each its event name and record) for `agent_id`, oldest
/// first, then keep only the agent's newest `ACTIVITY_PER_AGENT`.
pub fn post_activity(
    conn: &Connection,
    agent_id: i64,
    now: &str,
    events: &[(String, String)],
) -> Result<()> {
    let mut insert = conn.prepare(
        "INSERT INTO activity (agent_id, created_at, event, record) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (event, record) in events {
        insert.execute(params![agent_id, now, event, record])?;
    }
    conn.execute(
        "DELETE FROM activity WHERE agent_id = ?1 AND id <=
           (SELECT id FROM activity WHERE agent_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
        params![agent_id, ACTIVITY_PER_AGENT],
    )?;
    Ok(())
}

/// The newest `limit` events, oldest first; with `after`, the oldest `limit`
/// events after that id. With `agents`, only theirs.
pub fn activity_page(
    conn: &Connection,
    limit: u32,
    after: Option<i64>,
    agents: Option<&[i64]>,
) -> Result<Vec<ActivityRow>> {
    let (mut filter, order) = match after {
        Some(_) => ("a.id > ?2".to_string(), "ASC"),
        None => ("?2 IS NULL".to_string(), "DESC"),
    };
    if let Some(agents) = agents {
        let ids: Vec<String> = agents.iter().map(i64::to_string).collect();
        filter.push_str(&format!(" AND a.agent_id IN ({})", ids.join(", ")));
    }
    let sql = format!(
        "SELECT a.id, a.created_at, ag.name, h.name, a.record
         FROM activity a
         JOIN agents ag ON ag.id = a.agent_id
         LEFT JOIN agent_owners o ON o.agent_id = ag.id
         LEFT JOIN humans h ON h.id = o.human_id
         WHERE {filter}
         ORDER BY a.id {order} LIMIT ?1"
    );
    let mut rows = conn
        .prepare(&sql)?
        .query_map(params![limit, after], |r| {
            Ok(ActivityRow {
                id: r.get(0)?,
                created_at: r.get(1)?,
                agent: r.get(2)?,
                owner: r.get(3)?,
                record: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if after.is_none() {
        rows.reverse();
    }
    Ok(rows)
}

// ---- timers (timers.v1) ----------------------------------------------------

pub struct TimerRow {
    pub id: i64,
    pub target: String,
    pub creator: String,
    pub body: String,
    pub repeat: Option<String>,
    pub next_run: String,
}

/// A timer due to fire, with what firing it needs.
pub struct DueTimer {
    pub id: i64,
    pub target_id: i64,
    pub creator: Principal,
    pub body: String,
    pub repeat: Option<String>,
}

pub fn insert_timer(
    conn: &Connection,
    target_id: i64,
    creator: &Principal,
    body: &str,
    repeat: Option<&str>,
    next_run: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO timers (target_id, creator_kind, creator_id, body, repeat, next_run, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![target_id, creator.kind.as_str(), creator.id, body, repeat, next_run, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn timer_count(conn: &Connection, target_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM timers WHERE target_id = ?1",
        [target_id],
        |r| r.get(0),
    )?)
}

/// Timers, soonest first: one agent's, or with `None` everyone's.
pub fn timers(conn: &Connection, target_id: Option<i64>) -> Result<Vec<TimerRow>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, a.name, CASE t.creator_kind WHEN 'agent' THEN ca.name ELSE ch.name END,
                t.body, t.repeat, t.next_run
         FROM timers t
         JOIN agents a ON a.id = t.target_id
         LEFT JOIN agents ca ON t.creator_kind = 'agent' AND ca.id = t.creator_id
         LEFT JOIN humans ch ON t.creator_kind = 'human' AND ch.id = t.creator_id
         WHERE ?1 IS NULL OR t.target_id = ?1
         ORDER BY t.next_run, t.id",
    )?;
    let rows = stmt
        .query_map([target_id], |r| {
            Ok(TimerRow {
                id: r.get(0)?,
                target: r.get(1)?,
                creator: r.get(2)?,
                body: r.get(3)?,
                repeat: r.get(4)?,
                next_run: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// A timer's target, for checking who may remove it.
pub fn timer_target(conn: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT target_id FROM timers WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn delete_timer(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM timers WHERE id = ?1", [id])?;
    Ok(())
}

/// Timers due at `now`, oldest due first, at most `limit`.
pub fn due_timers(conn: &Connection, now: &str, limit: u32) -> Result<Vec<DueTimer>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.target_id, t.creator_kind, t.creator_id,
                CASE t.creator_kind WHEN 'agent' THEN ca.name ELSE ch.name END,
                t.body, t.repeat
         FROM timers t
         LEFT JOIN agents ca ON t.creator_kind = 'agent' AND ca.id = t.creator_id
         LEFT JOIN humans ch ON t.creator_kind = 'human' AND ch.id = t.creator_id
         WHERE t.next_run <= ?1
         ORDER BY t.next_run, t.id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![now, limit], |r| {
            let kind: String = r.get(2)?;
            Ok(DueTimer {
                id: r.get(0)?,
                target_id: r.get(1)?,
                creator: Principal {
                    kind: if kind == "human" {
                        Kind::Human
                    } else {
                        Kind::Agent
                    },
                    id: r.get(3)?,
                    name: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                },
                body: r.get(5)?,
                repeat: r.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// The soonest `next_run`, if any timer is set.
pub fn next_timer(conn: &Connection) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT min(next_run) FROM timers", [], |r| r.get(0))?)
}

pub fn set_timer_next_run(conn: &Connection, id: i64, next_run: &str) -> Result<()> {
    conn.execute(
        "UPDATE timers SET next_run = ?2 WHERE id = ?1",
        params![id, next_run],
    )?;
    Ok(())
}

pub fn set_message_timer(conn: &Connection, message_id: i64, timer_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE messages SET timer_id = ?2 WHERE id = ?1",
        params![message_id, timer_id],
    )?;
    Ok(())
}

pub fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "data_version", |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_agents_reports_last_sent_message_and_activity() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (1, 'Mateo', 'a', '/', 'h', 't', 't'), (2, 'Zed', 'b', '/', 'h', 't', 't')",
        )
        .unwrap();
        let last_sent = |name: &str| {
            list_agents(&conn, false)
                .unwrap()
                .into_iter()
                .find(|r| r.name == name)
                .and_then(|r| r.last_sent_at)
        };
        // Never sent: absent.
        assert_eq!(last_sent("Mateo"), None);
        // Mateo sends a message; an older activity event does not lower the max.
        conn.execute(
            "INSERT INTO messages (from_kind, from_id, body, created_at)
             VALUES ('agent', 1, 'hi', '2026-10-17T10:00:00Z')",
            [],
        )
        .unwrap();
        post_activity(&conn, 1, "2026-10-17T09:00:00Z", &[("text".into(), "e".into())]).unwrap();
        assert_eq!(last_sent("Mateo").as_deref(), Some("2026-10-17T10:00:00Z"));
        // Zed only posts activity.
        post_activity(&conn, 2, "2026-10-17T11:00:00Z", &[("text".into(), "e".into())]).unwrap();
        assert_eq!(last_sent("Zed").as_deref(), Some("2026-10-17T11:00:00Z"));
        // Trimming keeps the newest events, so the max survives it.
        let events = |n: i64| -> Vec<(String, String)> {
            (0..n).map(|i| ("text".into(), format!("e{i}"))).collect()
        };
        post_activity(&conn, 2, "2026-10-17T12:00:00Z", &events(ACTIVITY_PER_AGENT)).unwrap();
        post_activity(&conn, 2, "2026-10-17T13:00:00Z", &events(5)).unwrap();
        assert_eq!(last_sent("Zed").as_deref(), Some("2026-10-17T13:00:00Z"));
    }

    #[test]
    fn last_sent_lookups_do_not_grow_with_history() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO humans (id, name, created_at, active_at) VALUES (1, 'will', 't', 't');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (1, 'Mateo', 'a', '/', 'h', 't', 't')",
        )
        .unwrap();
        let fill = |conn: &Connection, n: usize| {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO messages (from_kind, from_id, body, created_at)
                     VALUES ('agent', 1, 'x', 't')",
                )
                .unwrap();
            for _ in 0..n {
                stmt.execute([]).unwrap();
            }
        };
        // SQLite VM ops one call takes, via the progress handler.
        let ops = |conn: &Connection, f: &dyn Fn(&Connection)| -> usize {
            let ops = Arc::new(AtomicUsize::new(0));
            let counting = ops.clone();
            conn.progress_handler(
                1,
                Some(move || {
                    counting.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .unwrap();
            f(conn);
            conn.progress_handler(0, None::<fn() -> bool>).unwrap();
            ops.load(Ordering::Relaxed)
        };
        fill(&conn, 1_000);
        let list_small = ops(&conn, &|c| {
            list_agents(c, false).unwrap();
        });
        let summary_small = ops(&conn, &|c| {
            agent_summaries(c, 1).unwrap();
        });
        fill(&conn, 99_000);
        let list_large = ops(&conn, &|c| {
            list_agents(c, false).unwrap();
        });
        let summary_large = ops(&conn, &|c| {
            agent_summaries(c, 1).unwrap();
        });
        // Each source's newest row comes from one bounded index walk, so a
        // 100x history grows the work by little, not by 100x.
        assert!(
            list_large < list_small * 2 + 1000,
            "list_agents: small={list_small} large={list_large}"
        );
        assert!(
            summary_large < summary_small * 2 + 1000,
            "agent_summaries: small={summary_small} large={summary_large}"
        );
    }

    #[test]
    fn activity_keeps_each_agents_newest_and_goes_when_it_retires() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (1, 'Mateo', 'a', '/', 'h', 't', 't'), (2, 'Zed', 'b', '/', 'h', 't', 't')",
        )
        .unwrap();
        let events = |n: i64, tag: &str| -> Vec<(String, String)> {
            (0..n)
                .map(|i| ("text".to_string(), format!("{tag}{i}")))
                .collect()
        };
        post_activity(&conn, 2, "t", &events(3, "zed")).unwrap();
        post_activity(&conn, 1, "t", &events(ACTIVITY_PER_AGENT, "a")).unwrap();
        post_activity(&conn, 1, "t", &events(5, "b")).unwrap();
        let count = |agent: i64| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM activity WHERE agent_id = ?1",
                [agent],
                |r| r.get(0),
            )
            .unwrap()
        };
        // Mateo's oldest five went; Zed's are untouched.
        assert_eq!((count(1), count(2)), (ACTIVITY_PER_AGENT, 3));
        let oldest = activity_page(&conn, 1, Some(0), None).unwrap();
        assert_eq!(oldest[0].record, "zed0");
        let mateo_oldest: String = conn
            .query_row(
                "SELECT record FROM activity WHERE agent_id = 1 ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mateo_oldest, "a5");
        let newest = activity_page(&conn, 2, None, None).unwrap();
        let zed = activity_page(&conn, 10, None, Some(&[2])).unwrap();
        let records: Vec<_> = zed.iter().map(|r| r.record.as_str()).collect();
        assert_eq!(records, ["zed0", "zed1", "zed2"]);
        let records: Vec<_> = newest.iter().map(|r| r.record.as_str()).collect();
        assert_eq!(records, ["b3", "b4"]);
        retire_agent(&conn, 1, "t").unwrap();
        assert_eq!((count(1), count(2)), (0, 3));
    }

    #[test]
    fn group_archive_migrates_existing_board_and_preserves_history() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in &MIGRATIONS[..5] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 5;
             INSERT INTO humans VALUES (1, 'will', '2026-10-14', '2026-10-14');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
                 VALUES (1, 'Aiko', 'token', '/', 'host', '2026-10-14', '2026-10-14');
             INSERT INTO bbs_groups VALUES (1, 'acme', '2026-10-14');
             INSERT INTO group_members VALUES (1, 1, '2026-10-14');
             INSERT INTO messages VALUES (1, 'human', 1, 1, 'old message', '2026-10-14');
             INSERT INTO deliveries VALUES (1, 'agent', 1, NULL);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(group_id(&conn, "acme").unwrap(), Some(1));
        assert_eq!(
            list_groups(&conn, None).unwrap(),
            // Existing members cannot write after migration.
            vec![GroupRow {
                name: "acme".into(),
                description: None,
                members: vec!["Aiko".into()],
                writers: vec![],
            }]
        );
        assert!(is_member(&conn, 1, 1).unwrap());
        assert_eq!(group_member_ids(&conn, 1).unwrap(), vec![1]);
        assert_eq!(member_groups(&conn, 1).unwrap(), vec![(1, "acme".into())]);
        archive_group(&conn, 1, "2026-10-15").unwrap();
        assert_eq!(group_id(&conn, "acme").unwrap(), None);

        assert!(!is_member(&conn, 1, 1).unwrap());
        assert!(!share_group(&conn, 1, 1).unwrap());
        assert!(group_member_ids(&conn, 1).unwrap().is_empty());
        assert!(member_groups(&conn, 1).unwrap().is_empty());
        assert!(agent_groups(&conn).unwrap().is_empty());
        assert!(list_groups(&conn, None).unwrap().is_empty());
        assert!(list_groups(&conn, Some(1)).unwrap().is_empty());
        let feed = feed_messages(&conn, 10, &FeedFilter::default()).unwrap();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].to_group.as_deref(), Some("acme"));
        assert_eq!(feed[0].body, "old message");
        let reader = Principal {
            kind: Kind::Agent,
            id: 1,
            name: "Aiko".into(),
        };
        let unread = take_unread(&conn, &reader, None, "2026-10-15").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].to_group.as_deref(), Some("acme"));
        assert!(
            !conn
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .exists([])
                .unwrap()
        );
        let index_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'bbs_groups_active_name'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(index_sql.contains("CREATE UNIQUE INDEX"));
        assert!(index_sql.contains("WHERE archived_at IS NULL"));
        let new_id = insert_group(&conn, "acme", None, "2026-10-15").unwrap();
        assert_ne!(new_id, 1);
        assert!(group_member_ids(&conn, new_id).unwrap().is_empty());
        assert!(insert_group(&conn, "acme", None, "2026-10-15").is_err());
        assert!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                .unwrap()
        );
    }

    #[test]
    fn group_description_migrates_existing_groups_and_limits_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in &MIGRATIONS[..6] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 6;
             INSERT INTO bbs_groups (id, name, created_at) VALUES (1, 'acme', '2026-10-14');
             INSERT INTO bbs_groups (id, name, created_at, archived_at)
                 VALUES (2, 'old', '2026-10-14', '2026-10-15');",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let row = |description: Option<&str>| GroupRow {
            name: "acme".into(),
            description: description.map(Into::into),
            members: vec![],
            writers: vec![],
        };
        assert_eq!(list_groups(&conn, None).unwrap(), vec![row(None)]);

        set_group_description(&conn, 1, Some("Infra work")).unwrap();
        assert_eq!(
            list_groups(&conn, None).unwrap(),
            vec![row(Some("Infra work"))]
        );
        // Character count, not bytes: 200 multi-byte characters fit.
        let limit = "\u{e9}".repeat(200);
        set_group_description(&conn, 1, Some(&limit)).unwrap();
        assert!(set_group_description(&conn, 1, Some(&format!("{limit}x"))).is_err());
        assert!(set_group_description(&conn, 1, Some("")).is_err());
        assert!(insert_group(&conn, "big", Some(&"x".repeat(201)), "2026-10-15").is_err());
        set_group_description(&conn, 1, None).unwrap();
        assert_eq!(list_groups(&conn, None).unwrap(), vec![row(None)]);

        // Archived groups keep their state; updates only reach active groups.
        set_group_description(&conn, 2, Some("ignored")).unwrap();
        let archived: Option<String> = conn
            .query_row("SELECT description FROM bbs_groups WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(archived, None);
        let id = insert_group(&conn, "new", Some("Fresh scope"), "2026-10-15").unwrap();
        assert_eq!(
            list_groups(&conn, None).unwrap()[1],
            GroupRow {
                name: "new".into(),
                description: Some("Fresh scope".into()),
                members: vec![],
                writers: vec![],
            }
        );
        assert_ne!(id, 1);
    }

    #[test]
    fn failed_migration_rolls_back_and_restores_foreign_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..5] {
            conn.execute_batch(sql).unwrap();
        }
        // An existing invalid reference must prevent publishing a rebuilt table.
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             PRAGMA user_version = 5;
             INSERT INTO messages VALUES (1, 'human', 1, 99, 'orphan', '2026-10-14');
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        assert!(
            migrate(&mut conn)
                .unwrap_err()
                .to_string()
                .contains("foreign-key")
        );
        assert_eq!(schema_version(&conn).unwrap(), 5);
        assert!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                .unwrap()
        );
        assert!(conn.prepare("SELECT archived_at FROM bbs_groups").is_err());
        assert!(
            !conn
                .prepare("SELECT name FROM sqlite_schema WHERE name = 'bbs_groups_replacement'")
                .unwrap()
                .exists([])
                .unwrap()
        );
    }

    #[test]
    fn feed_filters_use_indexes_not_a_message_scan() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let plan = |(sql, args): (String, Args)| -> Vec<String> {
            conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let group = FeedFilter {
            group_ids: Some(vec![1]),
            ..Default::default()
        };
        let group_cursor = FeedFilter {
            group_ids: Some(vec![1]),
            after_id: Some(10),
            ..Default::default()
        };
        let until = FeedFilter {
            until: Some("2026-01-01T00:00:00.000000Z"),
            ..Default::default()
        };
        let mine = FeedFilter {
            scope: Some(1),
            since: Some("2026-01-01T00:00:00.000000Z"),
            ..Default::default()
        };
        let cases = [
            (page_sql(200, &group), "messages_group"),
            (page_sql(200, &group_cursor), "messages_group"),
            (page_sql(200, &until), "messages_created"),
            (
                term_sql(Term::Sent(Kind::Agent, 2), 200, &mine, Expanded::Scope),
                "messages_sender",
            ),
            (
                term_sql(Term::DeliveredTo(2), 200, &mine, Expanded::Scope),
                "deliveries_recipient",
            ),
            (
                term_sql(Term::DirectTo(1), 200, &mine, Expanded::Scope),
                "deliveries_human_dm",
            ),
            (
                term_sql(Term::DirectToAgent(2), 200, &mine, Expanded::With),
                "deliveries_recipient",
            ),
            (
                term_sql(Term::ToGroup(1), 200, &mine, Expanded::Groups),
                "messages_group",
            ),
        ];
        for (sql, index) in cases {
            let plan = plan(sql);
            assert!(!plan.iter().any(|l| l == "SCAN m"), "{index}: {plan:?}");
            assert!(
                !plan.iter().any(|l| l.contains("TEMP B-TREE")),
                "{index}: {plan:?}"
            );
            assert!(plan.iter().any(|l| l.contains(index)), "{index}: {plan:?}");
        }
    }

    #[test]
    fn agent_summaries_walk_activity_by_agent_from_the_cursor() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {AGENT_SUMMARIES}"))
            .unwrap()
            .query_map([1], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Each of the two lookups, once per agent, ranges over the agent's
        // records after the cursor; nothing scans the activity table.
        let activity: Vec<_> = plan.iter().filter(|l| l.contains(" act ")).collect();
        assert_eq!(activity.len(), 2, "{plan:#?}");
        for line in activity {
            assert!(
                line.starts_with("SEARCH act USING")
                    && line.contains("activity_agent (agent_id=? AND id>?)"),
                "{plan:#?}"
            );
        }
        // The cursor is one primary-key lookup per agent.
        let cursor = "SEARCH s USING PRIMARY KEY (human_id=? AND agent_id=?)";
        assert!(plan.iter().any(|l| l.starts_with(cursor)), "{plan:#?}");
        // The last-sent lookups use the sender and per-agent activity indexes.
        assert!(
            plan.iter()
                .any(|l| l.contains("messages_sender (from_kind=? AND from_id=?)")),
            "{plan:#?}"
        );
        assert!(
            plan.iter()
                .any(|l| l.starts_with("SEARCH activity USING INDEX activity_agent (agent_id=?)")),
            "{plan:#?}"
        );
    }

    #[test]
    fn human_conversations_backfill_like_incremental_recording() {
        let pairs = |conn: &Connection| -> Vec<(i64, i64, String)> {
            conn.prepare("SELECT agent_id, human_id, last_at FROM agent_human_conversations")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let t = |s: u32| format!("2026-10-19T00:00:{s:02}.000000Z");
        // Humans 1-2; agents 1-2 live, 3 a retired "Mateo" and 4 the live
        // "Mateo" registered after it; group 1. Each message: (id, from kind,
        // from id, group, recipient kind, recipient id, control, timer, second).
        type Row = (
            i64,
            &'static str,
            i64,
            Option<i64>,
            &'static str,
            i64,
            Option<&'static str>,
            Option<i64>,
            u32,
        );
        let history: [Row; 11] = [
            (1, "human", 1, None, "agent", 1, None, None, 1), // will -> 1
            (2, "agent", 1, None, "human", 1, None, None, 5), // 1 -> will, newer
            (3, "human", 1, None, "agent", 1, None, None, 3), // older again
            (4, "agent", 2, None, "human", 2, None, None, 2), // 2 -> ada
            (5, "human", 2, None, "agent", 3, None, None, 4), // ada -> retired 3
            (6, "human", 1, None, "agent", 2, Some("cancel"), None, 9),
            (7, "human", 1, None, "agent", 2, None, Some(1), 10), // timer
            (8, "human", 1, Some(1), "agent", 2, None, None, 11), // broadcast
            (9, "agent", 1, Some(1), "human", 2, None, None, 12), // broadcast
            (10, "agent", 1, None, "agent", 2, None, None, 13),   // agent to agent
            (
                11,
                "agent",
                2,
                None,
                "human",
                1,
                Some("set-effort"),
                None,
                14,
            ),
        ];
        let qualifies = |r: &Row| r.3.is_none() && r.6.is_none() && r.7.is_none() && r.1 != r.4;
        let setup = |conn: &Connection| {
            conn.execute_batch(
                "INSERT INTO humans (id, name, created_at, active_at) VALUES
                     (1, 'will', 'x', 'x'), (2, 'ada', 'x', 'x');
                 INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
                 VALUES (1, 'Bram', 't1', '/', 'h', 'x', 'x', NULL),
                        (2, 'Zed', 't2', '/', 'h', 'x', 'x', NULL),
                        (3, 'Mateo', 't3', '/', 'h', 'x', 'x', 'x'),
                        (4, 'Mateo', 't4', '/', 'h', 'x', 'x', NULL);
                 INSERT INTO bbs_groups (id, name, created_at) VALUES (1, 'crew', 'x');",
            )
            .unwrap();
        };
        let insert = |conn: &Connection, r: &Row| {
            conn.execute(
                "INSERT INTO messages (id, from_kind, from_id, to_group_id, body, created_at, control, timer_id)
                 VALUES (?1, ?2, ?3, ?4, 'x', ?5, ?6, ?7)",
                params![r.0, r.1, r.2, r.3, t(r.8), r.6, r.7],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO deliveries (message_id, recipient_kind, recipient_id, direct)
                 VALUES (?1, ?2, ?3, ?4)",
                params![r.0, r.4, r.5, r.3.is_none()],
            )
            .unwrap();
        };

        // Backfill: the history exists at schema 15, then the board migrates.
        let mut old = Connection::open_in_memory().unwrap();
        old.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in &MIGRATIONS[..15] {
            old.execute_batch(sql).unwrap();
        }
        old.pragma_update(None, "user_version", 15).unwrap();
        setup(&old);
        history.iter().for_each(|r| insert(&old, r));
        migrate(&mut old).unwrap();

        // Incremental: the same history recorded as it is sent.
        let mut new = Connection::open_in_memory().unwrap();
        migrate(&mut new).unwrap();
        setup(&new);
        for r in &history {
            insert(&new, r);
            if qualifies(r) {
                let (agent, human) = if r.1 == "agent" {
                    (r.2, r.5)
                } else {
                    (r.5, r.2)
                };
                record_human_conversation(&new, agent, human, &t(r.8)).unwrap();
            }
        }
        let mut backfilled = pairs(&old);
        backfilled.sort();
        let mut recorded = pairs(&new);
        recorded.sort();
        assert_eq!(backfilled, recorded);
        // Newest per pair, never moved back by older mail; nothing else counts.
        assert_eq!(backfilled, [(1, 1, t(5)), (2, 2, t(2)), (3, 2, t(4))]);

        // The agent's value is its newest pair; a new agent reusing a retired
        // one's name does not inherit its pairs.
        record_human_conversation(&old, 1, 2, &t(7)).unwrap();
        record_human_conversation(&old, 1, 2, &t(6)).unwrap();
        let talked: Vec<_> = list_agents(&old, true)
            .unwrap()
            .into_iter()
            .map(|a| (a.id, a.last_human_conversation_at))
            .collect();
        assert!(talked.contains(&(1, Some(t(7)))));
        assert!(talked.contains(&(2, Some(t(2)))));
        assert!(talked.contains(&(3, Some(t(4)))));
        assert!(talked.contains(&(4, None)));
        let human = insert_human(&old, "lee", &t(0)).unwrap();
        let summaries: Vec<_> = agent_summaries(&old, human)
            .unwrap()
            .into_iter()
            .map(|s| (s.agent, s.last_human_conversation_at))
            .collect();
        assert_eq!(
            summaries,
            [
                ("Bram".into(), Some(t(7))),
                ("Mateo".into(), None),
                ("Zed".into(), Some(t(2)))
            ]
        );

        // A rolled-back send leaves no record.
        let tx = old.transaction().unwrap();
        record_human_conversation(&tx, 2, 1, &t(30)).unwrap();
        drop(tx);
        assert!(!pairs(&old).iter().any(|p| p.0 == 2 && p.1 == 1));
    }

    #[test]
    fn feed_indexes_migrate_existing_board_and_keep_history() {
        let index_names = |conn: &Connection| -> Vec<String> {
            conn.prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'index' AND name IN
                 ('messages_group', 'messages_sender', 'messages_created',
                  'deliveries_human_dm', 'deliveries_recipient') ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };
        // A pre-merge build of this change may already have created the indexes.
        for preexisting in [false, true] {
            let mut conn = Connection::open_in_memory().unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            for sql in &MIGRATIONS[..7] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "PRAGMA user_version = 7;
                 INSERT INTO humans (id, name, created_at, active_at)
                     VALUES (1, 'will', '2026-10-14', '2026-10-14');
                 INSERT INTO bbs_groups (id, name, created_at) VALUES (1, 'acme', '2026-10-14');
                 INSERT INTO messages (id, from_kind, from_id, to_group_id, body, created_at)
                     VALUES (1, 'human', 1, 1, 'before schema 8', '2026-10-14T00:00:00.000000Z');",
            )
            .unwrap();
            if preexisting {
                conn.execute_batch(
                    "CREATE INDEX messages_group ON messages (to_group_id, id);
                     CREATE INDEX deliveries_recipient
                         ON deliveries (recipient_kind, recipient_id, message_id);",
                )
                .unwrap();
            }
            assert!(index_names(&conn).len() < 5);
            migrate(&mut conn).unwrap();
            migrate(&mut conn).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
            assert_eq!(index_names(&conn).len(), 5, "preexisting: {preexisting}");
            let filter = FeedFilter {
                group_ids: Some(vec![1]),
                ..Default::default()
            };
            let feed = feed_messages(&conn, 10, &filter).unwrap();
            assert_eq!(feed.len(), 1);
            assert_eq!(feed[0].body, "before schema 8");
        }
    }

    #[test]
    fn migration_13_flags_existing_direct_deliveries() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in &MIGRATIONS[..12] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 12;
             INSERT INTO humans (id, name, created_at, active_at)
                 VALUES (1, 'will', '2026-10-14', '2026-10-14'),
                        (2, 'ada', '2026-10-14', '2026-10-14');
             INSERT INTO bbs_groups (id, name, created_at) VALUES (1, 'acme', '2026-10-14');
             INSERT INTO messages (id, from_kind, from_id, to_group_id, body, created_at)
                 VALUES (1, 'human', 2, NULL, 'direct', '2026-10-14T00:00:00.000000Z'),
                        (2, 'human', 2, 1, 'broadcast', '2026-10-14T00:00:01.000000Z');
             INSERT INTO deliveries (message_id, recipient_kind, recipient_id)
                 VALUES (1, 'human', 1), (2, 'human', 1);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let direct: Vec<(i64, bool)> = conn
            .prepare("SELECT message_id, direct FROM deliveries ORDER BY message_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(direct, [(1, true), (2, false)]);
        let filter = FeedFilter {
            scope: Some(1),
            ..Default::default()
        };
        let bodies: Vec<_> = feed_messages(&conn, 10, &filter)
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(bodies, ["direct"]);
    }

    #[test]
    fn controls_are_delivered_live_but_never_feed_or_history_content() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let t = "2026-10-09T00:00:00.000000Z";
        let will = Principal {
            kind: Kind::Human,
            id: insert_human(&conn, "will", t).unwrap(),
            name: "will".into(),
        };
        let bram = Principal {
            kind: Kind::Agent,
            id: insert_agent(
                &conn,
                &NewAgent {
                    name: "Bram",
                    token: "t",
                    owner_id: will.id,
                    status: None,
                    pwd: "/",
                    hostname: "h",
                    now: t,
                },
            )
            .unwrap(),
            name: "Bram".into(),
        };
        let ordinary = insert_message(&conn, &will, None, "hi Bram", t).unwrap();
        insert_delivery(&conn, ordinary, Kind::Agent, bram.id).unwrap();
        // Controls straddle the ordinary message in time: one older (pins
        // archived history's oldest-first limit), one newer (pins the feed's
        // newest-first limit). A post-LIMIT filter would fail either window.
        let cancel = insert_message(&conn, &will, None, "cancel", "2026-10-08T23:59:58.000000Z")
            .unwrap();
        set_message_control(&conn, cancel, "cancel").unwrap();
        insert_delivery(&conn, cancel, Kind::Agent, bram.id).unwrap();
        let effort = insert_message(
            &conn,
            &will,
            None,
            "set-effort",
            "2026-10-09T00:00:02.000000Z",
        )
        .unwrap();
        set_message_control(&conn, effort, "set-effort").unwrap();
        insert_delivery(&conn, effort, Kind::Agent, bram.id).unwrap();
        // Default feed, cursor continuation, the agent thread (--with), and
        // one-row windows all exclude controls without crowding out mail.
        for filter in [
            FeedFilter::default(),
            FeedFilter {
                after_id: Some(0),
                ..Default::default()
            },
            FeedFilter {
                with: Some(vec![(Kind::Agent, bram.id)]),
                ..Default::default()
            },
        ] {
            let bodies: Vec<_> = feed_messages(&conn, 10, &filter)
                .unwrap()
                .into_iter()
                .map(|m| m.body)
                .collect();
            assert_eq!(bodies, ["hi Bram"]);
        }
        let bodies: Vec<_> = feed_messages(&conn, 1, &FeedFilter::default())
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(bodies, ["hi Bram"]);
        // Live delivery still hands both controls to the driver, and report
        // correlation still finds the set-effort one.
        let delivered: Vec<_> = take_unread(&conn, &bram, None, "2026-10-09T01:00:00.000000Z")
            .unwrap()
            .into_iter()
            .map(|m| (m.body, m.control))
            .collect();
        assert_eq!(
            delivered,
            [
                ("cancel".to_string(), Some("cancel".to_string())),
                ("hi Bram".to_string(), None),
                ("set-effort".to_string(), Some("set-effort".to_string())),
            ]
        );
        assert!(effort_control_body(&conn, bram.id, ordinary).unwrap().is_none());
        assert_eq!(
            effort_control_body(&conn, bram.id, effort).unwrap().as_deref(),
            Some("set-effort")
        );
        // Archived history excludes controls too, oldest-first and limited.
        let history: Vec<_> = archived_since(&conn, &bram, None, None)
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(history, ["hi Bram"]);
        let history: Vec<_> = archived_since(&conn, &bram, None, Some(1))
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(history, ["hi Bram"], "the oldest row is a control");
    }

    #[test]
    fn feed_shows_broadcasts_once_and_dm_read_times() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let t = "2026-10-09T00:00:00.000000Z";
        let will = Principal {
            kind: Kind::Human,
            id: insert_human(&conn, "will", t).unwrap(),
            name: "will".into(),
        };
        let agent = |name: &str, token: &str| {
            let a = NewAgent {
                name,
                token,
                owner_id: will.id,
                status: None,
                pwd: "/",
                hostname: "h",
                now: t,
            };
            Principal {
                kind: Kind::Agent,
                id: insert_agent(&conn, &a).unwrap(),
                name: name.into(),
            }
        };
        let (a, b, c) = (
            agent("Aiko", "t1"),
            agent("Bram", "t2"),
            agent("Cian", "t3"),
        );
        let g = insert_group(&conn, "acme", None, t).unwrap();

        let m = insert_message(&conn, &a, Some(g), "hello all", t).unwrap();
        for p in [&b, &c, &will] {
            insert_delivery(&conn, m, p.kind, p.id).unwrap();
        }
        let m = insert_message(&conn, &will, None, "hi Bram", t).unwrap();
        insert_delivery(&conn, m, Kind::Agent, b.id).unwrap();

        let feed = feed_messages(&conn, 10, &FeedFilter::default()).unwrap();
        assert_eq!(feed.len(), 2);
        assert_eq!(
            (feed[0].to_group.as_deref(), feed[0].to_name.as_deref()),
            (Some("acme"), None)
        );
        assert_eq!(feed[0].read_at, None);
        assert_eq!(feed[1].from_name, "will");
        assert_eq!(feed[1].to_name.as_deref(), Some("Bram"));
        assert_eq!(feed[1].read_at, None);

        take_unread(&conn, &b, None, "2026-10-09T01:00:00.000000Z").unwrap();
        let feed = feed_messages(&conn, 10, &FeedFilter::default()).unwrap();
        assert_eq!(feed[0].read_at, None, "broadcast read times stay hidden");
        assert_eq!(
            feed[1].read_at.as_deref(),
            Some("2026-10-09T01:00:00.000000Z")
        );
    }
}
