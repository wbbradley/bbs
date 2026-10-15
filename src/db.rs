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
}

#[derive(Debug, PartialEq, Eq)]
pub struct GroupRow {
    pub name: String,
    pub description: Option<String>,
    pub members: Vec<String>,
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
    /// Feed only: when a direct message was read; always `None` for broadcasts.
    pub read_at: Option<String>,
    pub body: String,
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

/// Retire an agent: free its name, archive its unread mail, drop its memberships.
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
    Ok(())
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
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.status, a.pwd, a.hostname, a.active_at, a.retired_at, h.name
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
        "SELECT g.id, g.name, g.description, a.name FROM bbs_groups g
         LEFT JOIN group_members m ON m.group_id = g.id
         LEFT JOIN agents a ON a.id = m.agent_id
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
        ))
    })?;
    for row in rows {
        let (id, name, description, member) = row?;
        if groups.last().is_none_or(|(g, _)| *g != id) {
            let group = GroupRow {
                name,
                description,
                members: Vec::new(),
            };
            groups.push((id, group));
        }
        if let Some(member) = member {
            groups.last_mut().unwrap().1.members.push(member);
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

pub fn insert_delivery(conn: &Connection, message_id: i64, kind: Kind, id: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO deliveries (message_id, recipient_kind, recipient_id) VALUES (?1, ?2, ?3)",
        params![message_id, kind.as_str(), id],
    )?;
    Ok(())
}

const INBOX_SELECT: &str = "
    SELECT m.id, m.created_at, m.from_kind,
           CASE m.from_kind WHEN 'agent' THEN a.name ELSE h.name END,
           g.name, m.body, owner.name, reader_owner.name
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
        read_at: None,
        body: r.get(5)?,
    })
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

/// Fetch unread messages oldest-first and mark them archived. `from_kind`
/// restricts this to messages sent by humans or by agents.
pub fn take_unread(
    conn: &Connection,
    me: &Principal,
    from_kind: Option<Kind>,
    limit: Option<u32>,
    now: &str,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{INBOX_SELECT} AND d.archived_at IS NULL AND (?3 IS NULL OR m.from_kind = ?3)
         ORDER BY m.created_at, m.id LIMIT ?4"
    );
    let limit = limit.map_or(i64::MAX, i64::from);
    let from_kind = from_kind.map(Kind::as_str);
    let msgs: Vec<MessageView> = conn
        .prepare(&sql)?
        .query_map(
            params![me.kind.as_str(), me.id, from_kind, limit],
            message_view,
        )?
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
        "{INBOX_SELECT} AND d.archived_at IS NOT NULL AND (?3 IS NULL OR m.created_at >= ?3)
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
    /// A direct message to this human. Agents cannot message humans, so only
    /// human-sent direct messages qualify.
    DirectTo(i64),
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
        Term::ToGroup(group) => (
            format!(
                "SELECT m.id FROM messages m WHERE m.to_group_id = ?{predicates}
                 ORDER BY m.id {dir} LIMIT ?"
            ),
            vec![Value::Integer(group)],
        ),
        Term::DirectTo(human) => (
            format!(
                "SELECT m.id FROM messages m INDEXED BY messages_human_dm
                 WHERE m.to_group_id IS NULL AND m.from_kind = 'human'
                   AND EXISTS (SELECT 1 FROM deliveries x WHERE x.message_id = m.id
                               AND x.recipient_kind = 'human' AND x.recipient_id = ?){predicates}
                 ORDER BY m.id {dir} LIMIT ?"
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
    let (expanded, terms) = if let Some(senders) = &filter.senders {
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
                COALESCE(ta.name, th.name), d.archived_at, m.body, owner.name
         FROM messages m
         LEFT JOIN agents fa ON m.from_kind = 'agent' AND fa.id = m.from_id
         LEFT JOIN humans fh ON m.from_kind = 'human' AND fh.id = m.from_id
         LEFT JOIN agent_owners sender ON sender.agent_id = fa.id
         LEFT JOIN humans owner ON owner.id = sender.human_id
         LEFT JOIN bbs_groups g ON g.id = m.to_group_id
         LEFT JOIN deliveries d ON m.to_group_id IS NULL AND d.message_id = m.id
         LEFT JOIN agents ta ON d.recipient_kind = 'agent' AND ta.id = d.recipient_id
         LEFT JOIN humans th ON d.recipient_kind = 'human' AND th.id = d.recipient_id
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
                read_at: r.get(6)?,
                body: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "data_version", |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            vec![GroupRow {
                name: "acme".into(),
                description: None,
                members: vec!["Aiko".into()],
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
        let unread = take_unread(&conn, &reader, None, None, "2026-10-15").unwrap();
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
                "messages_human_dm",
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
    fn feed_indexes_migrate_existing_board_and_keep_history() {
        let index_names = |conn: &Connection| -> Vec<String> {
            conn.prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'index' AND name IN
                 ('messages_group', 'messages_sender', 'messages_created',
                  'messages_human_dm', 'deliveries_recipient') ORDER BY name",
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
            assert_eq!(schema_version(&conn).unwrap(), 8);
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

        take_unread(&conn, &b, None, None, "2026-10-09T01:00:00.000000Z").unwrap();
        let feed = feed_messages(&conn, 10, &FeedFilter::default()).unwrap();
        assert_eq!(feed[0].read_at, None, "broadcast read times stay hidden");
        assert_eq!(
            feed[1].read_at.as_deref(),
            Some("2026-10-09T01:00:00.000000Z")
        );
    }
}
