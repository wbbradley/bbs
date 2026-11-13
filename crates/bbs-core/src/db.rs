//! All SQL lives here, as plain functions over a `Connection`. Keep queries to
//! portable SQL so this module is the only thing that changes for another backend.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

pub mod blockers;
mod hub_model;
pub mod integration;
pub mod notes;

pub use hub_model::{
    EntityMapping, HUB_MODEL_SCHEMA_VERSION, HUB_MODEL_SOURCE_VERSION, HubMigrationReport,
    PreflightReport, hub_model_preflight, lock_exclusive, migrate_to_hub_model, namespace,
    validate_namespace,
};

use crate::entity::{AddressRef, DEFAULT_NAMESPACE, EntityKind, EntityRef, NATIVE_ADDRESS};

/// The schema this binary serves, and creates new databases at.
pub const SCHEMA_VERSION: usize = HUB_MODEL_SCHEMA_VERSION;

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
    include_str!("../migrations/0018_side_channels.sql"),
    include_str!("../migrations/0019_side_questions.sql"),
    include_str!("../migrations/0020_side_conversations.sql"),
    include_str!("../migrations/0021_blockers.sql"),
    include_str!("../migrations/0022_agent_notes.sql"),
    include_str!("../migrations/0023_agent_drains.sql"),
    include_str!("../migrations/0024_blocker_choices.sql"),
    include_str!("../migrations/0025_agent_focus.sql"),
    include_str!("../migrations/0026_agent_goal.sql"),
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
    /// When its owner put it in focus mode; None when not focused.
    pub focused_at: Option<String>,
    /// The newest validated goal report (JSON) and when the server got it.
    pub goal: Option<String>,
    pub goal_reported_at: Option<String>,
    /// The newest activity event that starts, continues or ends a turn (prompt,
    /// steer, tool, thought, text, turn_end, turn_error, cancel or stop); None
    /// when there is none.
    pub turn_event: Option<String>,
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
    /// The sender is another installation's entity; `from_kind` is then
    /// meaningless and `from_name` is its `id:ENTITY@NAMESPACE` reference.
    pub from_remote: bool,
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
    /// A group copy (D127): its original sender. `from_name` stays the group,
    /// which sent the copy.
    pub original_from: Option<String>,
    /// A group copy the group itself sent: there is no original sender.
    pub group_notice: bool,
    /// With `original_from`: it is in another namespace than the group.
    pub reported: bool,
    /// The message this one answers: its ID here, or its origin
    /// `NAMESPACE:MESSAGE_ID` when this installation does not have it.
    pub in_reply_to: Option<String>,
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    // A database `migrate` will refuse stays exactly as it is, without WAL
    // files, so its owner can still back it up and migrate it.
    let version = schema_version(&conn)?;
    if version == 0 || version == SCHEMA_VERSION {
        enable_wal(&conn)?;
    }
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

/// Open a database at [`SCHEMA_VERSION`], creating a new (version 0) one in
/// [`DEFAULT_NAMESPACE`]. See [`migrate_in`].
pub fn migrate(conn: &mut Connection) -> Result<()> {
    migrate_in(conn, None)
}

/// Every command calls this on open. A database at [`SCHEMA_VERSION`] is ready;
/// with `namespace`, its namespace must match. A new database (version 0) is
/// created at [`SCHEMA_VERSION`] in `namespace`, else [`DEFAULT_NAMESPACE`];
/// concurrent callers serialize on the write lock and re-check the version
/// under it. Any other version is refused, unchanged: only the operator's
/// `bbs-server migrate-hub-model` moves existing data to the Hub model.
pub fn migrate_in(conn: &mut Connection, namespace: Option<&str>) -> Result<()> {
    if let Some(namespace) = namespace {
        validate_namespace(namespace)?;
    }
    if schema_version(conn)? == 0 {
        create(conn, namespace.unwrap_or(DEFAULT_NAMESPACE))?;
    }
    check_supported(conn, schema_version(conn)?)?;
    let has_namespace: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'bbs_installation')",
        [],
        |r| r.get(0),
    )?;
    if !has_namespace {
        bail!(
            "this schema-v{SCHEMA_VERSION} database has no installation namespace \
             (bbs_installation): an earlier development build of `bbs-server migrate-hub-model` \
             made it. Restore the schema-v{HUB_MODEL_SOURCE_VERSION} backup taken before that \
             and migrate it again with this release"
        );
    }
    let missing = missing_integration_schema(conn)?;
    if !missing.is_empty() {
        bail!(
            "this schema-v{SCHEMA_VERSION} database lacks the integration schema ({}): an \
             earlier development build of the Hub model made it. If `bbs-server \
             migrate-hub-model` made it, restore the schema-v{HUB_MODEL_SOURCE_VERSION} backup \
             taken before that and migrate it again with this release; if it was created \
             new, move it aside and let this release create it",
            missing.join(", ")
        );
    }
    let stored = hub_model::namespace(conn)?;
    if let Some(namespace) = namespace
        && namespace != stored
    {
        bail!("this database's namespace is {stored:?}, not {namespace:?}");
    }
    Ok(())
}

/// The tables and columns of the Hub model's integration schema that this
/// schema-27 database lacks; early development builds of migration 27 had
/// none of them.
fn missing_integration_schema(conn: &Connection) -> Result<Vec<String>> {
    const REQUIRED: &[(&str, &str)] = &[
        ("bbs_installation", "catalog_epoch"),
        ("bbs_integrations", "publication_revision"),
        ("bbs_integrations", "publication_digest"),
        ("integration_tokens", "token_hash"),
        ("integration_namespaces", "integration_id"),
        ("messages", "group_copy_namespace"),
        ("messages", "group_copy_message_id"),
        ("messages", "group_copy_sender_address_id"),
        ("messages", "group_copy_sender_run_id"),
    ];
    let mut missing = Vec::new();
    for (table, column) in REQUIRED {
        let present: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            params![table, column],
            |r| r.get(0),
        )?;
        if !present {
            missing.push(format!("{table}.{column}"));
        }
    }
    Ok(missing)
}

/// Refuse a schema this binary cannot serve.
fn check_supported(conn: &Connection, version: usize) -> Result<()> {
    if version > SCHEMA_VERSION {
        bail!("BBS schema v{version} is newer than this binary supports");
    }
    let path = conn
        .path()
        .filter(|p| !p.is_empty())
        .unwrap_or("STATE_DIR/bbs.db");
    let state_dir = Path::new(path)
        .parent()
        .map_or("STATE_DIR".into(), |p| p.display().to_string());
    let version_text = crate::release::VERSION;
    let steps = format!(
        "Stop every process using it (`bbs serve`, local `bbs` commands, headless runs and \
         `bbs connect`), then run:\n\
         \x20 bbs backup {state_dir}/pre-hub.db --state-dir {state_dir}\n\
         \x20 cargo install --locked bbs-server --version {version_text}   # if bbs-server is not installed\n\
         \x20 bbs-server migrate-hub-model --db {path} --namespace bbs --backup-taken {state_dir}/pre-hub.db\n\
         --namespace is this installation's permanent Hub namespace, [a-z][a-z0-9-]*; \
         keep `bbs`, the default for new databases, unless it will federate under another name. \
         Add --dry-run first to see the entity IDs it will assign."
    );
    if version == HUB_MODEL_SOURCE_VERSION {
        bail!(
            "{path} is at BBS schema v{version}, before the Hub model (v{SCHEMA_VERSION}); \
             this release does not migrate it automatically. {steps}"
        );
    }
    if version < HUB_MODEL_SOURCE_VERSION {
        bail!(
            "{path} is at BBS schema v{version}, too old to migrate to the Hub model \
             (v{SCHEMA_VERSION}) directly; open it once with bbs 0.24 to reach schema \
             v{HUB_MODEL_SOURCE_VERSION}. Then: {steps}"
        );
    }
    Ok(())
}

/// Create a new database: every migration, then the Hub model, in one
/// transaction. Does nothing if another connection created it first.
fn create(conn: &mut Connection, namespace: &str) -> Result<()> {
    validate_namespace(namespace)?;
    with_foreign_keys_off(conn, |tx| {
        if schema_version(tx)? != 0 {
            return Ok(false);
        }
        apply_migrations(tx, 0)?;
        hub_model::create_in(tx, namespace)?;
        tracing::info!(namespace, "created database");
        Ok(true)
    })
}

/// Bring a database before the Hub model up to schema 26, as bbs 0.24 did on
/// every open. Only tests use it, to build older databases.
#[doc(hidden)]
pub fn migrate_legacy(conn: &mut Connection) -> Result<()> {
    let version = schema_version(conn)?;
    if version > HUB_MODEL_SOURCE_VERSION {
        bail!("BBS schema v{version} is past the legacy migrations");
    }
    if version == HUB_MODEL_SOURCE_VERSION {
        return Ok(());
    }
    with_foreign_keys_off(conn, |tx| {
        let current = schema_version(tx)?;
        if current > HUB_MODEL_SOURCE_VERSION {
            bail!("BBS schema v{current} is past the legacy migrations");
        }
        apply_migrations(tx, current)?;
        Ok(true)
    })
}

/// Apply `MIGRATIONS` after `current` and set schema 26.
fn apply_migrations(tx: &Connection, current: usize) -> Result<()> {
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
        tracing::info!(version = i + 1, "applied migration");
    }
    tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    Ok(())
}

/// Run `f` in one write transaction with foreign-key enforcement off, as
/// SQLite table rebuilds require; the setting can only change outside a
/// transaction. When `f` changed something (returns true), validate the
/// references before commit. Restores the caller's setting even if `f` or the
/// commit fails.
fn with_foreign_keys_off(
    conn: &mut Connection,
    f: impl FnOnce(&Connection) -> Result<bool>,
) -> Result<()> {
    if !conn.is_autocommit() {
        bail!("BBS migrations require a connection without an active transaction");
    }
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if f(&tx)? && tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
            bail!("BBS migration would leave invalid foreign-key references");
        }
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
    let entity_id = hub_model::new_entity_id(conn, EntityKind::Human, name, None)?;
    conn.execute(
        "INSERT INTO humans (name, created_at, active_at) VALUES (?1, ?2, ?2)",
        params![name, now],
    )?;
    let id = conn.last_insert_rowid();
    hub_model::create_entity(conn, EntityKind::Human, id, &entity_id, name, now)?;
    Ok(id)
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

/// Retire an agent: free its name, archive its unread mail, drop its memberships
/// and write grants.
pub fn retire_agent(conn: &Connection, id: i64, now: &str) -> Result<()> {
    let retired = conn.execute(
        "UPDATE agents SET retired_at = ?1 WHERE id = ?2 AND retired_at IS NULL",
        params![now, id],
    )?;
    // Notes retire with their agent, in this transaction, so every affected
    // human's sync and summary signal see it.
    if retired > 0 {
        notes::retire(conn, id, now)?;
        hub_model::retire_entity(conn, EntityKind::Agent, id)?;
    }
    conn.execute(
        &format!(
            "UPDATE deliveries SET archived_at = ?1
             WHERE recipient_address_id = {} AND recipient_run_id = '' AND archived_at IS NULL",
            address_sql(EntityKind::Agent, "?2")
        ),
        params![now, id],
    )?;
    conn.execute("DELETE FROM group_members WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM group_writers WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM activity WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM timers WHERE target_id = ?1", params![id])?;
    conn.execute("DELETE FROM activity_seen WHERE agent_id = ?1", params![id])?;
    conn.execute("DELETE FROM side_state WHERE agent_id = ?1", params![id])?;
    conn.execute(
        "DELETE FROM owner_input_requests WHERE agent_id = ?1",
        params![id],
    )?;
    // No longer used, but a server rolled back to older code would show
    // a retired agent's side questions again.
    conn.execute("DELETE FROM side_requests WHERE agent_id = ?1", params![id])?;
    conn.execute(
        "UPDATE agents SET effort = NULL, effort_reported_at = NULL, focused_at = NULL,
                goal = NULL, goal_reported_at = NULL
         WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Start (`Some(now)`) or end (`None`) focus mode for a live agent; returns
/// whether it changed.
pub fn set_focus(conn: &Connection, id: i64, now: Option<&str>) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE agents SET focused_at = ?2
         WHERE id = ?1 AND retired_at IS NULL AND (focused_at IS NULL) = (?2 IS NOT NULL)",
        params![id, now],
    )?;
    Ok(changed > 0)
}

/// When the agent's focus mode began; None when it is not focused.
pub fn focused_at(conn: &Connection, id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT focused_at FROM agents WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Store an agent's validated effort report JSON and when it arrived.
pub fn set_effort(conn: &Connection, id: i64, report: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET effort = ?2, effort_reported_at = ?3 WHERE id = ?1",
        params![id, report, now],
    )?;
    Ok(())
}

/// Store an agent's validated goal report JSON (None: it has no goal) and
/// when it arrived. The time stays set either way: it says a goal-capable
/// runner reported.
pub fn set_goal(conn: &Connection, id: i64, report: Option<&str>, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET goal = ?2, goal_reported_at = ?3 WHERE id = ?1",
        params![id, report, now],
    )?;
    Ok(())
}

/// The body of message `id` when it is a set-effort control delivered to
/// agent `agent`.
pub fn effort_control_body(conn: &Connection, agent: i64, id: i64) -> Result<Option<String>> {
    conn.query_row(
        &format!(
            "SELECT m.body FROM deliveries d JOIN messages m ON m.id = d.message_id
             WHERE d.recipient_address_id = {} AND m.id = ?2 AND m.control = 'set-effort'",
            address_sql(EntityKind::Agent, "?1")
        ),
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
    let entity_id = hub_model::new_entity_id(conn, EntityKind::Agent, a.name, Some(a.owner_id))?;
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
    hub_model::create_entity(conn, EntityKind::Agent, id, &entity_id, a.name, a.now)?;
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
    let mut stmt = conn.prepare(&format!(
        "SELECT a.id, a.name, a.status, a.pwd, a.hostname, a.active_at, a.retired_at, h.name,
                a.effort, a.effort_reported_at,
                (SELECT MAX(created_at) FROM (
                   SELECT created_at FROM (
                     SELECT created_at FROM messages
                     WHERE sender_address_id = {} AND kind = 'application'
                     ORDER BY id DESC LIMIT 1)
                   UNION ALL
                   SELECT created_at FROM (
                     SELECT created_at FROM activity
                     WHERE agent_id = a.id
                     ORDER BY id DESC LIMIT 1))),
                (SELECT max(last_at) FROM agent_human_conversations p WHERE p.agent_id = a.id),
                a.focused_at, a.goal, a.goal_reported_at,
                (SELECT event FROM activity
                 WHERE agent_id = a.id AND event IN ('prompt', 'steer', 'tool', 'thought', 'text', 'turn_end', 'turn_error', 'cancel', 'stop')
                 ORDER BY id DESC LIMIT 1)
         FROM agents a
         LEFT JOIN agent_owners o ON o.agent_id = a.id
         LEFT JOIN humans h ON h.id = o.human_id
         WHERE a.retired_at IS NULL OR ?1
         ORDER BY a.retired_at IS NOT NULL, lower(a.name), a.id",
        address_sql(EntityKind::Agent, "a.id")
    ))?;
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
                focused_at: r.get(12)?,
                goal: r.get(13)?,
                goal_reported_at: r.get(14)?,
                turn_event: r.get(15)?,
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
    let entity_id = hub_model::new_entity_id(conn, EntityKind::Group, name, None)?;
    conn.execute(
        "INSERT INTO bbs_groups (name, description, created_at) VALUES (?1, ?2, ?3)",
        params![name, description, now],
    )?;
    let id = conn.last_insert_rowid();
    hub_model::create_entity(conn, EntityKind::Group, id, &entity_id, name, now)?;
    Ok(id)
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
    let archived = conn.execute(
        "UPDATE bbs_groups SET archived_at = ?1 WHERE id = ?2 AND archived_at IS NULL",
        params![now, group_id],
    )?;
    if archived > 0 {
        hub_model::retire_entity(conn, EntityKind::Group, group_id)?;
    }
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

/// The group's live members in focus mode: (id, name).
pub fn focused_members(conn: &Connection, group_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name FROM group_members m JOIN agents a ON a.id = m.agent_id
         WHERE m.group_id = ?1 AND a.retired_at IS NULL AND a.focused_at IS NOT NULL
         ORDER BY lower(a.name)",
    )?;
    let members = stmt
        .query_map(params![group_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(members)
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

// ---- entities and addresses ------------------------------------------------

impl From<Kind> for EntityKind {
    fn from(kind: Kind) -> EntityKind {
        match kind {
            Kind::Agent => EntityKind::Agent,
            Kind::Human => EntityKind::Human,
        }
    }
}

/// SQL for the native address (an `entity_addresses` row id) of the native
/// record of `kind` whose id is the SQL expression `id`. Messages and
/// deliveries refer to these; a parameter `id` makes it a constant the
/// planner evaluates once.
pub(crate) fn address_sql(kind: EntityKind, id: &str) -> String {
    format!(
        "({})",
        addresses_sql(&format!("nb.{} = {id}", hub_model::binding_column(kind)))
    )
}

/// SQL selecting the native addresses of the native records whose binding
/// `nb` matches the SQL condition `condition`.
fn addresses_sql(condition: &str) -> String {
    format!(
        "SELECT na.id FROM entity_local_bindings nb
         JOIN entity_addresses na ON na.entity_id = nb.entity_id AND na.address_id = '{NATIVE_ADDRESS}'
         WHERE {condition}"
    )
}

/// Joins naming message `m`'s sender: its address `sa` and entity `se`, and
/// for a native sender its binding `sb` and the agent `fa` or human `fh` it
/// is. A foreign sender has no binding.
const SENDER_JOINS: &str = "
    JOIN entity_addresses sa ON sa.id = m.sender_address_id
    JOIN entities se ON se.id = sa.entity_id
    LEFT JOIN entity_local_bindings sb ON sb.entity_id = sa.entity_id
    LEFT JOIN agents fa ON fa.id = sb.agent_id
    LEFT JOIN humans fh ON fh.id = sb.human_id";

/// The sender's kind, with [`SENDER_JOINS`]: `remote` for another
/// installation's entity, whatever an adapter says it is.
const SENDER_KIND: &str = "CASE WHEN sb.agent_id IS NOT NULL THEN 'agent'
    WHEN sb.human_id IS NOT NULL THEN 'human' ELSE 'remote' END";

/// The sender's name, with [`SENDER_JOINS`]: a native name, or a foreign
/// sender's exact address as `id:ENTITY@NAMESPACE` (see
/// [`crate::entity::remote_display`]), which a reply can name.
static SENDER_NAME: LazyLock<String> = LazyLock::new(|| {
    format!(
        "coalesce(fa.name, fh.name, {})",
        remote_display_sql("se", "sa", "m.sender_run_id")
    )
});

/// Joins naming a group copy's original sender (D127): its address `oa` and
/// entity `oe`, with [`SENDER_JOINS`]. Only imports store copies, and import
/// refuses a copy whose original is this installation's, so the original
/// sender is always another installation's entity.
const COPY_JOINS: &str = "
    LEFT JOIN entity_addresses oa ON m.group_copy_message_id IS NOT NULL
      AND oa.id = m.group_copy_sender_address_id
    LEFT JOIN entities oe ON oe.id = oa.entity_id";

/// Columns read by [`copy_view`], with [`SENDER_JOINS`] and [`COPY_JOINS`]: a
/// copy's original sender, whether the group sent it itself (a notice),
/// whether the original sender is in another namespace than the group, and
/// what the message answers. A remote sender names what it answers, so the
/// answered message's local ID is given only when the reader whose address
/// is the SQL `reader` sent it or had it delivered; otherwise only its
/// origin. Without `reader` (the board-wide feed, which shows every message)
/// any local message counts.
fn copy_columns(reader: Option<&str>) -> String {
    let visible = match reader {
        Some(a) => format!(
            "AND (r.sender_address_id = {a} OR EXISTS (SELECT 1 FROM deliveries rd
               WHERE rd.message_id = r.id AND rd.recipient_address_id = {a}))"
        ),
        None => String::new(),
    };
    format!(
        "CASE WHEN oe.id IS NULL THEN NULL ELSE {} END, oe.id = se.id, oe.namespace != se.namespace,
         m.in_reply_to_namespace, m.in_reply_to_message_id,
         (SELECT r.id FROM messages r WHERE r.origin_namespace = m.in_reply_to_namespace
            AND r.origin_message_id = m.in_reply_to_message_id {visible})",
        remote_display_sql("oe", "oa", "coalesce(m.group_copy_sender_run_id, '')")
    )
}

/// Apply [`copy_columns`], starting at column `at`, to `view`: a group copy
/// stays from the group and names its original sender, unless the group sent
/// it itself (a notice).
fn copy_view(mut view: MessageView, r: &rusqlite::Row, at: usize) -> rusqlite::Result<MessageView> {
    if let Some(original) = r.get::<_, Option<String>>(at)? {
        if r.get::<_, Option<bool>>(at + 1)? == Some(true) {
            view.group_notice = true;
        } else {
            view.original_from = Some(original);
            view.reported = r.get::<_, Option<bool>>(at + 2)? == Some(true);
        }
    }
    let namespace: Option<String> = r.get(at + 3)?;
    let message_id: Option<String> = r.get(at + 4)?;
    let local: Option<i64> = r.get(at + 5)?;
    view.in_reply_to = match (local, namespace, message_id) {
        (Some(id), _, _) => Some(id.to_string()),
        (None, Some(ns), Some(id)) => Some(format!("{ns}:{id}")),
        _ => None,
    };
    Ok(view)
}

/// SQL for [`crate::entity::remote_display`] of entity `e`, address `a` and
/// the run expression `run`.
pub(crate) fn remote_display_sql(e: &str, a: &str, run: &str) -> String {
    format!(
        "'id:' || {e}.entity_id || '@' || {e}.namespace
         || CASE WHEN {a}.address_id = '{NATIVE_ADDRESS}' THEN '' ELSE ':' || {a}.address_id END
         || CASE WHEN {run} = '' THEN '' ELSE '#' || {run} END"
    )
}

/// Move every retired entity off its plain ID, as the cutover does for records
/// retired before it, so tests can reuse a retired native name.
#[cfg(test)]
pub(crate) fn date_retired_entity_ids(conn: &Connection) {
    conn.execute(
        "UPDATE entities SET entity_id = entity_id || '-retired-2026-10-01'
         WHERE lifecycle = 'retired' AND entity_id NOT LIKE '%-retired-%'",
        [],
    )
    .unwrap();
}

/// A native record's entity and native address; None when it has none.
pub fn native_address(conn: &Connection, kind: EntityKind, id: i64) -> Result<Option<AddressRef>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT e.namespace, e.entity_id, a.address_id
                 FROM entity_addresses a JOIN entities e ON e.id = a.entity_id
                 WHERE a.id = {}",
                address_sql(kind, "?1")
            ),
            [id],
            |r| {
                Ok(AddressRef {
                    entity: EntityRef {
                        namespace: r.get(0)?,
                        entity_id: r.get(1)?,
                    },
                    address_id: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// The `entity_addresses` row of a native record's native address.
pub fn native_address_row(conn: &Connection, kind: EntityKind, id: i64) -> Result<i64> {
    conn.query_row(&format!("SELECT {}", address_sql(kind, "?1")), [id], |r| {
        r.get::<_, Option<i64>>(0)
    })?
    .context("no native address")
}

/// The entity IDs of every native record of `kind`, by native ID.
pub fn native_entity_ids(conn: &Connection, kind: EntityKind) -> Result<HashMap<i64, String>> {
    let column = hub_model::binding_column(kind);
    let mut stmt = conn.prepare(&format!(
        "SELECT nb.{column}, e.entity_id FROM entity_local_bindings nb
         JOIN entities e ON e.id = nb.entity_id WHERE nb.{column} IS NOT NULL"
    ))?;
    let ids = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// A native record's entity ID; None when it has no entity binding.
pub fn native_entity_id(conn: &Connection, kind: EntityKind, id: i64) -> Result<Option<String>> {
    let column = hub_model::binding_column(kind);
    Ok(conn
        .query_row(
            &format!(
                "SELECT e.entity_id FROM entity_local_bindings nb
                 JOIN entities e ON e.id = nb.entity_id WHERE nb.{column} = ?1"
            ),
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// A native entity of this installation, found by its ID.
#[derive(Debug)]
pub struct LocalEntity {
    /// Its exact entity ID.
    pub entity_id: String,
    /// The human or agent it is; None for a group.
    pub principal: Option<Principal>,
    /// The entity and the named address are both active.
    pub active: bool,
}

/// The native entity `entity_id` of this installation, with its address
/// `address_id`. New entity IDs are unique ignoring case, so case is
/// ignored; the table's own constraint is exact, so IDs differing only in
/// case are refused as ambiguous rather than one picked.
pub fn local_entity(
    conn: &Connection,
    entity_id: &str,
    address_id: &str,
) -> Result<Option<LocalEntity>> {
    let mut found = conn
        .prepare(
            "SELECT e.entity_id, e.lifecycle = 'active' AND a.lifecycle = 'active',
                    nb.human_id, h.name, nb.agent_id, ag.name
             FROM entities e
             JOIN entity_local_bindings nb ON nb.entity_id = e.id
             JOIN entity_addresses a ON a.entity_id = e.id AND a.address_id = ?3
             LEFT JOIN humans h ON h.id = nb.human_id
             LEFT JOIN agents ag ON ag.id = nb.agent_id
             WHERE e.namespace = ?1 AND lower(e.entity_id) = lower(?2)
             LIMIT 2",
        )?
        .query_map(params![namespace(conn)?, entity_id, address_id], |r| {
            let human: Option<i64> = r.get(2)?;
            let agent: Option<i64> = r.get(4)?;
            let principal = match (human, agent) {
                (Some(id), _) => Some(Principal {
                    kind: Kind::Human,
                    id,
                    name: r.get(3)?,
                }),
                (None, Some(id)) => Some(Principal {
                    kind: Kind::Agent,
                    id,
                    name: r.get(5)?,
                }),
                (None, None) => None,
            };
            Ok(LocalEntity {
                entity_id: r.get(0)?,
                principal,
                active: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if found.len() > 1 {
        bail!("entity ID {entity_id} matches several entities that differ only in case");
    }
    Ok(found.pop())
}

// ---- messages --------------------------------------------------------------

/// Where a message goes.
#[derive(Debug, Clone, Copy)]
pub enum To {
    /// A broadcast; its deliveries follow through [`insert_delivery`].
    Group(i64),
    /// A direct message to one human or agent.
    Direct(Kind, i64),
}

/// A new native message's ID in the installation's namespace: opaque and
/// random, never the `bbs-legacy-` prefix the cutover gave older messages.
/// Integration handoff IDs use the same form.
pub(crate) fn new_message_id() -> String {
    let bytes: [u8; 16] = rand::random();
    data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase()
}

/// Store a message from `from` and, when direct, its one delivery.
pub fn insert_message(
    conn: &Connection,
    from: &Principal,
    to: To,
    body: &str,
    now: &str,
) -> Result<i64> {
    let (kind, to_id, group) = match to {
        To::Group(id) => (EntityKind::Group, id, Some(id)),
        To::Direct(kind, id) => (kind.into(), id, None),
    };
    let inserted = conn.execute(
        &format!(
            "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
                 recipient_address_id, to_group_id, body, created_at, authored_at)
             SELECT namespace, ?1, {}, {}, ?4, ?5, ?6, ?6 FROM bbs_installation",
            address_sql(from.kind.into(), "?2"),
            address_sql(kind, "?3"),
        ),
        params![new_message_id(), from.id, to_id, group, body, now],
    )?;
    if inserted != 1 {
        bail!("this database has no installation namespace");
    }
    let id = conn.last_insert_rowid();
    if let To::Direct(kind, to_id) = to {
        insert_delivery(conn, id, kind, to_id)?;
    }
    Ok(id)
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

/// Deliver a broadcast to one recipient.
pub fn insert_delivery(conn: &Connection, message_id: i64, kind: Kind, id: i64) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO deliveries (message_id, recipient_address_id, direct)
             SELECT id, {}, to_group_id IS NULL FROM messages WHERE id = ?1",
            address_sql(kind.into(), "?2")
        ),
        params![message_id, id],
    )?;
    Ok(())
}

/// Mark message `id` as an automatic owner reply.
pub fn set_message_auto_reply(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE messages SET auto_reply = 1 WHERE id = ?1", [id])?;
    Ok(())
}

/// Record that native message `id` answers the message with origin
/// `(namespace, message_id)`.
pub fn set_message_in_reply_to(
    conn: &Connection,
    id: i64,
    namespace: &str,
    message_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE messages SET in_reply_to_namespace = ?2, in_reply_to_message_id = ?3
         WHERE id = ?1",
        params![id, namespace, message_id],
    )?;
    Ok(())
}

/// Who a reply goes to.
#[derive(Debug)]
pub enum ReplyTarget {
    /// A native human or agent.
    Native(Principal),
    /// Another installation's exact address, run included, as
    /// [`crate::entity::remote_display`] names it.
    Remote(String),
    /// A group copy's notice, which the group sent itself: there is no
    /// sender to reply to.
    GroupNotice,
}

/// What [`reply_source`] finds about a message being answered.
#[derive(Debug)]
pub struct ReplySource {
    /// The caller sent it.
    pub mine: bool,
    /// It was delivered to the caller.
    pub delivered: bool,
    pub target: ReplyTarget,
    /// The answered message's origin, `(namespace, message ID)`: a group
    /// copy's original, else the message's own.
    pub answers: (String, String),
}

/// Who a reply by `me` to message `id` goes to, following Hub's rule
/// (`hubclient.ReplyTo`): a group copy's original sender, answering the
/// original message, unless the group itself sent it (a notice); otherwise
/// the message's exact sender, answering the message. None when there is no
/// such message.
pub fn reply_source(conn: &Connection, me: &Principal, id: i64) -> Result<Option<ReplySource>> {
    let me_address = address_sql(me.kind.into(), "?2");
    let row = conn
        .query_row(
            &format!(
                "SELECT m.sender_address_id = {me_address},
                    EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.id
                      AND d.recipient_address_id = {me_address} AND d.recipient_run_id = ''),
                    m.origin_namespace, m.origin_message_id, m.sender_address_id, m.sender_run_id,
                    m.group_copy_namespace, m.group_copy_message_id, oa.id,
                    coalesce(m.group_copy_sender_run_id, ''), oa.entity_id = sa.entity_id
                 FROM messages m
                 JOIN entity_addresses sa ON sa.id = m.sender_address_id
                 LEFT JOIN entity_addresses oa ON m.group_copy_message_id IS NOT NULL
                   AND oa.id = m.group_copy_sender_address_id
                 WHERE m.id = ?1"
            ),
            params![id, me.id],
            |r| {
                Ok((
                    r.get::<_, bool>(0)?,
                    r.get::<_, bool>(1)?,
                    (r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    (r.get::<_, i64>(4)?, r.get::<_, String>(5)?),
                    (
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, Option<bool>>(10)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((mine, delivered, origin, (sender, sender_run), copy)) = row else {
        return Ok(None);
    };
    let (target, answers) = match copy {
        (_, _, _, _, Some(true)) => (ReplyTarget::GroupNotice, origin),
        (Some(namespace), Some(message_id), Some(by), run, _) => {
            (address_target(conn, by, &run)?, (namespace, message_id))
        }
        _ => (address_target(conn, sender, &sender_run)?, origin),
    };
    Ok(Some(ReplySource {
        mine,
        delivered,
        target,
        answers,
    }))
}

/// Who `entity_addresses` row `address` with run `run` is, to reply to.
fn address_target(conn: &Connection, address: i64, run: &str) -> Result<ReplyTarget> {
    conn.query_row(
        "SELECT e.namespace, e.entity_id, a.address_id, fa.id, fa.name, fh.id, fh.name
         FROM entity_addresses a JOIN entities e ON e.id = a.entity_id
         LEFT JOIN entity_local_bindings b ON b.entity_id = e.id
         LEFT JOIN agents fa ON fa.id = b.agent_id
         LEFT JOIN humans fh ON fh.id = b.human_id
         WHERE a.id = ?1",
        [address],
        |r| {
            let principal = |kind, at| -> rusqlite::Result<Option<Principal>> {
                Ok(r.get::<_, Option<i64>>(at)?
                    .zip(r.get(at + 1)?)
                    .map(|(id, name)| Principal { kind, id, name }))
            };
            Ok(
                match (principal(Kind::Agent, 3)?, principal(Kind::Human, 5)?) {
                    (Some(p), _) | (_, Some(p)) => ReplyTarget::Native(p),
                    _ => ReplyTarget::Remote(crate::entity::remote_display(
                        &r.get::<_, String>(0)?,
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        run,
                    )),
                },
            )
        },
    )
    .map_err(Into::into)
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
            "SELECT ar.message_id, m.body, rb.human_id
             FROM auto_replies ar
             JOIN messages m ON m.id = ar.message_id
             JOIN deliveries d ON d.message_id = ar.message_id
             JOIN entity_addresses ra ON ra.id = d.recipient_address_id
             JOIN entity_local_bindings rb ON rb.entity_id = ra.entity_id AND rb.human_id IS NOT NULL
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

// ---- side channels (agent-interactions.v1) -------------------------------------

/// One agent's side state: its current generation, latest reported usage
/// (JSON), and the side-state revision counter.
pub struct SideState {
    pub generation: String,
    pub usage: Option<String>,
    pub reported_at: String,
    pub revision: i64,
    /// Requests at or below this sequence are closed, published or not.
    pub closed_seq: i64,
}

pub fn side_state(conn: &Connection, agent_id: i64) -> Result<Option<SideState>> {
    Ok(conn
        .query_row(
            "SELECT generation, usage, reported_at, revision, closed_seq
             FROM side_state WHERE agent_id = ?1",
            [agent_id],
            |r| {
                Ok(SideState {
                    generation: r.get(0)?,
                    usage: r.get(1)?,
                    reported_at: r.get(2)?,
                    revision: r.get(3)?,
                    closed_seq: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Begin `agent`'s interaction session with `generation`. Idempotent for the
/// current value; a new value resets side state: usage cleared, pending input
/// requests marked stale, revision bumped. Returns whether a reset happened.
pub fn begin_side_session(
    conn: &Connection,
    agent_id: i64,
    generation: &str,
    now: &str,
) -> Result<bool> {
    let current = side_state(conn, agent_id)?;
    if let Some(current) = &current
        && current.generation == generation
    {
        return Ok(false);
    }
    let revision = current.map_or(1, |s| s.revision + 1);
    conn.execute(
        "INSERT INTO side_state (agent_id, generation, usage, supported, reason, followups, reported_at, revision, closed_seq)
         VALUES (?1, ?2, NULL, NULL, NULL, NULL, ?3, ?4, 0)
         ON CONFLICT (agent_id) DO UPDATE SET
           generation = excluded.generation, usage = NULL, supported = NULL,
           reason = NULL, followups = NULL, reported_at = excluded.reported_at,
           revision = excluded.revision, closed_seq = 0",
        params![agent_id, generation, now, revision],
    )?;
    // Nothing here reads the side columns or side_requests any more. Clearing
    // them keeps a server rolled back to older code from offering side
    // questions to this runner or waiting on an open one.
    conn.execute(
        "UPDATE side_requests SET status = 'interrupted', updated_at = ?2
         WHERE agent_id = ?1 AND status = 'open'",
        params![agent_id, now],
    )?;
    conn.execute(
        "UPDATE owner_input_requests SET status = 'stale', updated_at = ?2
         WHERE agent_id = ?1 AND status = 'pending'",
        params![agent_id, now],
    )?;
    Ok(true)
}

/// Require `generation` to be the agent's current one.
pub fn check_generation(conn: &Connection, agent_id: i64, generation: &str) -> Result<bool> {
    Ok(side_state(conn, agent_id)?.is_some_and(|s| s.generation == generation))
}

/// Replace the agent's reported context usage: usage JSON, or clear.
/// With `preserve_usage`, usage and its report time stay untouched.
pub fn report_context(
    conn: &Connection,
    agent_id: i64,
    usage: Option<&str>,
    now: &str,
    preserve_usage: bool,
) -> Result<()> {
    if preserve_usage {
        return Ok(());
    }
    conn.execute(
        "UPDATE side_state SET usage = ?2, reported_at = ?3, revision = revision + 1
         WHERE agent_id = ?1",
        params![agent_id, usage, now],
    )?;
    Ok(())
}

/// One owner-input request's stored row.
pub struct OwnerInputRow {
    pub id: i64,
    pub generation: String,
    pub seq: i64,
    pub form: String,
    pub answers: Option<String>,
    pub status: String,
    pub updated_at: String,
}

fn owner_input_row(r: &rusqlite::Row) -> rusqlite::Result<OwnerInputRow> {
    Ok(OwnerInputRow {
        id: r.get(0)?,
        generation: r.get(1)?,
        seq: r.get(2)?,
        form: r.get(3)?,
        answers: r.get(4)?,
        status: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

const OWNER_INPUT_SELECT: &str =
    "SELECT id, generation, seq, form, answers, status, updated_at FROM owner_input_requests";

/// The agent's pending request, if any (at most one by construction).
pub fn pending_owner_input(conn: &Connection, agent_id: i64) -> Result<Option<OwnerInputRow>> {
    Ok(conn
        .query_row(
            &format!("{OWNER_INPUT_SELECT} WHERE agent_id = ?1 AND status = 'pending'"),
            [agent_id],
            owner_input_row,
        )
        .optional()?)
}

/// The agent's newest request regardless of status.
pub fn latest_owner_input(conn: &Connection, agent_id: i64) -> Result<Option<OwnerInputRow>> {
    Ok(conn
        .query_row(
            &format!("{OWNER_INPUT_SELECT} WHERE agent_id = ?1 ORDER BY id DESC LIMIT 1"),
            [agent_id],
            owner_input_row,
        )
        .optional()?)
}

/// One request by id, for exact-id answers and closes.
pub fn owner_input(conn: &Connection, id: i64) -> Result<Option<OwnerInputRow>> {
    Ok(conn
        .query_row(
            &format!("{OWNER_INPUT_SELECT} WHERE id = ?1"),
            [id],
            owner_input_row,
        )
        .optional()?)
}

/// Store a new pending request at `seq`, pruning prior terminal rows so only
/// the latest request is kept. Returns its never-reused id.
pub fn insert_owner_input(
    conn: &Connection,
    agent_id: i64,
    generation: &str,
    seq: i64,
    form: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "DELETE FROM owner_input_requests WHERE agent_id = ?1 AND status != 'pending'",
        [agent_id],
    )?;
    conn.execute(
        "INSERT INTO owner_input_requests (agent_id, generation, seq, form, answers, status, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, 'pending', ?5)",
        params![agent_id, generation, seq, form, now],
    )?;
    conn.execute(
        "UPDATE side_state SET revision = revision + 1 WHERE agent_id = ?1",
        [agent_id],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Raise the closed-sequence barrier; a delayed publish at or below it is rejected.
pub fn close_owner_input_seq(conn: &Connection, agent_id: i64, seq: i64) -> Result<()> {
    conn.execute(
        "UPDATE side_state SET closed_seq = max(closed_seq, ?2), revision = revision + 1
         WHERE agent_id = ?1",
        params![agent_id, seq],
    )?;
    Ok(())
}

/// Set a request's terminal status and answers.
pub fn finish_owner_input(
    conn: &Connection,
    id: i64,
    answers: Option<&str>,
    status: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE owner_input_requests SET answers = ?2, status = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, answers, status, now],
    )?;
    conn.execute(
        "UPDATE side_state SET revision = revision + 1
         WHERE agent_id = (SELECT agent_id FROM owner_input_requests WHERE id = ?1)",
        [id],
    )?;
    Ok(())
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

/// The inbox of `me`, a principal of `kind` whose id is ?2 (?1 is the kind).
fn inbox_select(kind: Kind) -> String {
    format!(
        "SELECT m.id, m.created_at, {SENDER_KIND}, {},
                g.name, m.body, owner.name, reader_owner.name, m.control, m.timer_id, m.auto_reply,
                {}
         FROM deliveries d
         JOIN messages m ON m.id = d.message_id
         {SENDER_JOINS}
         {COPY_JOINS}
         LEFT JOIN agent_owners sender ON sender.agent_id = fa.id
         LEFT JOIN humans owner ON owner.id = sender.human_id
         LEFT JOIN agent_owners reader ON ?1 = 'agent' AND reader.agent_id = ?2
         LEFT JOIN humans reader_owner ON reader_owner.id = reader.human_id
         LEFT JOIN bbs_groups g ON g.id = m.to_group_id
         WHERE d.recipient_address_id = {} AND d.recipient_run_id = ''",
        *SENDER_NAME,
        copy_columns(Some("d.recipient_address_id")),
        address_sql(kind.into(), "?2")
    )
}

fn message_view(r: &rusqlite::Row) -> rusqlite::Result<MessageView> {
    let from_kind: String = r.get(2)?;
    let view = MessageView {
        id: r.get(0)?,
        created_at: r.get(1)?,
        from_kind: if from_kind == "human" {
            Kind::Human
        } else {
            Kind::Agent
        },
        from_remote: from_kind == "remote",
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
        original_from: None,
        group_notice: false,
        reported: false,
        in_reply_to: None,
    };
    copy_view(view, r, 11)
}

/// Mark read `me`'s unread mail from one sender; returns how many.
pub fn mark_read_from(
    conn: &Connection,
    me: &Principal,
    from: &Principal,
    now: &str,
) -> Result<usize> {
    Ok(conn.execute(
        &mark_read_from_sql(me.kind, from.kind),
        params![now, me.kind.as_str(), me.id, from.id],
    )?)
}

/// [`mark_read_from`]'s update: ?1 the time, ?2 and ?3 the reader's kind and
/// id, ?4 the sender's id.
fn mark_read_from_sql(me: Kind, from: Kind) -> String {
    format!(
            "UPDATE deliveries SET archived_at = ?1
             WHERE recipient_address_id = {} AND recipient_run_id = '' AND archived_at IS NULL
               AND message_id IN (SELECT id FROM messages WHERE sender_address_id = {})
               AND (?2 != 'agent'
                 OR EXISTS (SELECT 1 FROM messages m
                   WHERE m.id = deliveries.message_id AND m.control = 'cancel')
                 OR message_id <= coalesce((
                 SELECT r.watermark FROM agent_drains r JOIN agent_owners o ON o.human_id = r.human_id
                 WHERE o.agent_id = ?3), 9223372036854775807))",
            address_sql(me.into(), "?3"),
            address_sql(from.into(), "?4"),
        )
}

/// What a human has yet to look at from one agent, and when it last sent.
#[derive(Debug, PartialEq)]
pub struct AgentSummary {
    pub agent_id: i64,
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
    /// When its owner put it in focus mode; None when not focused.
    pub focused_at: Option<String>,
    /// The newest validated goal report (JSON) and when the server got it.
    pub goal: Option<String>,
    pub goal_reported_at: Option<String>,
    /// The newest activity event that starts, continues or ends a turn (prompt,
    /// steer, tool, thought, text, turn_end, turn_error, cancel or stop); None
    /// when there is none.
    pub turn_event: Option<String>,
}

/// Unread mail through the human's inbox index; unseen activity per live
/// agent through `activity_agent`, from the human's cursor, so neither walks
/// the whole activity table. Last-sent times come from `messages_sender` and
/// `activity_agent` the same way.
static AGENT_SUMMARIES: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH unread AS (
            SELECT sb.agent_id AS agent_id, count(*) AS n, min(m.created_at) AS oldest
            FROM deliveries d JOIN messages m ON m.id = d.message_id
            JOIN entity_addresses sa ON sa.id = m.sender_address_id
            JOIN entity_local_bindings sb ON sb.entity_id = sa.entity_id
            WHERE d.recipient_address_id = {} AND d.recipient_run_id = ''
              AND d.archived_at IS NULL AND d.direct = 1 AND sb.agent_id IS NOT NULL
            GROUP BY sb.agent_id),
        summaries AS MATERIALIZED (
            SELECT g.id AS agent_id, g.name, coalesce(u.n, 0) AS unread, u.oldest AS unread_oldest,
                   g.effort, g.effort_reported_at, g.focused_at, g.goal, g.goal_reported_at,
                   (SELECT count(*) FROM activity act
                    WHERE act.agent_id = g.id AND act.id > coalesce(s.seen_id, 0)) AS unseen,
                   -- Ids grow with arrival, so the first unseen is the oldest.
                   (SELECT act.created_at FROM activity act
                    WHERE act.agent_id = g.id AND act.id > coalesce(s.seen_id, 0)
                    ORDER BY act.id LIMIT 1) AS unseen_oldest,
                   (SELECT MAX(created_at) FROM (
                       SELECT created_at FROM (
                         SELECT created_at FROM messages
                         WHERE sender_address_id = {} AND kind = 'application'
                         ORDER BY id DESC LIMIT 1)
                       UNION ALL
                       SELECT created_at FROM (
                         SELECT created_at FROM activity
                         WHERE agent_id = g.id
                         ORDER BY id DESC LIMIT 1))) AS last_sent,
                   (SELECT max(p.last_at) FROM agent_human_conversations p
                    WHERE p.agent_id = g.id) AS human_conversation,
                   (SELECT turn.event FROM activity turn
                    WHERE turn.agent_id = g.id AND turn.event IN ('prompt', 'steer', 'tool', 'thought', 'text', 'turn_end', 'turn_error', 'cancel', 'stop')
                    ORDER BY turn.id DESC LIMIT 1) AS turn_event
            FROM agents g
            LEFT JOIN activity_seen s ON s.human_id = ?1 AND s.agent_id = g.id
            LEFT JOIN unread u ON u.agent_id = g.id
            WHERE g.retired_at IS NULL)
        SELECT name, unread, unread_oldest, unseen, unseen_oldest, last_sent, effort,
               effort_reported_at, human_conversation, agent_id, focused_at, goal,
               goal_reported_at, turn_event FROM summaries
        ORDER BY lower(name)",
        address_sql(EntityKind::Human, "?1"),
        address_sql(EntityKind::Agent, "g.id"),
    )
});

/// For each live agent, the direct messages it sent `human` that are unread,
/// the shared activity `human` has not seen, and when it last sent anything.
/// Broadcasts don't count as unread: humans receive every one.
pub fn agent_summaries(conn: &Connection, human: i64) -> Result<Vec<AgentSummary>> {
    let mut stmt = conn.prepare_cached(&AGENT_SUMMARIES)?;
    let rows = stmt
        .query_map([human], |r| {
            Ok(AgentSummary {
                agent_id: r.get(9)?,
                agent: r.get(0)?,
                unread_messages: r.get(1)?,
                oldest_unread_message: r.get(2)?,
                unseen_stream: r.get(3)?,
                oldest_unseen_stream: r.get(4)?,
                last_sent_at: r.get(5)?,
                effort: r.get(6)?,
                effort_reported_at: r.get(7)?,
                last_human_conversation_at: r.get(8)?,
                focused_at: r.get(10)?,
                goal: r.get(11)?,
                goal_reported_at: r.get(12)?,
                turn_event: r.get(13)?,
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

/// Mail a focused agent may take: its owner's direct messages (controls
/// included), its own timers, and answers to its blockers from any human.
/// Mail sent during focus is refused or not delivered at all; this holds what
/// was already waiting when focus began until it ends. Expects the message as `m`, the recipient
/// kind as ?1 and its id as ?2.
static FOCUS_ALLOWS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "(?1 != 'agent'
         OR (SELECT focused_at FROM agents WHERE id = ?2) IS NULL
         OR (m.to_group_id IS NULL AND m.sender_address_id = {})
         OR m.sender_address_id = {}
         OR EXISTS (SELECT 1 FROM blocker_events e
             WHERE e.message_id = m.id AND e.kind = 'answered'))",
        address_sql(
            EntityKind::Human,
            "(SELECT human_id FROM agent_owners WHERE agent_id = ?2)"
        ),
        address_sql(EntityKind::Agent, "?2"),
    )
});

pub fn has_unread(conn: &Connection, me: &Principal) -> Result<bool> {
    let sql = has_unread_sql(me.kind);
    let n: i64 = conn.query_row(&sql, params![me.kind.as_str(), me.id], |r| r.get(0))?;
    Ok(n != 0)
}

/// [`has_unread`]'s query: ?1 and ?2 the reader's kind and id.
fn has_unread_sql(kind: Kind) -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM deliveries
         WHERE recipient_address_id = {} AND recipient_run_id = '' AND archived_at IS NULL
         AND EXISTS (SELECT 1 FROM messages m WHERE m.id = deliveries.message_id
           AND {})
         AND (?1 != 'agent'
           OR EXISTS (SELECT 1 FROM messages m
               WHERE m.id = deliveries.message_id AND m.control = 'cancel')
           OR message_id <= coalesce((
           SELECT r.watermark FROM agent_drains r JOIN agent_owners o ON o.human_id = r.human_id
           WHERE o.agent_id = ?2), 9223372036854775807)))",
        address_sql(kind.into(), "?2"),
        *FOCUS_ALLOWS,
    )
}

/// Fetch unread messages oldest-first and mark them archived.
pub fn take_unread(
    conn: &Connection,
    me: &Principal,
    limit: Option<u32>,
    now: &str,
) -> Result<Vec<MessageView>> {
    let sql = take_unread_sql(me.kind);
    let limit = limit.map_or(i64::MAX, i64::from);
    let msgs: Vec<MessageView> = conn
        .prepare(&sql)?
        .query_map(params![me.kind.as_str(), me.id, limit], message_view)?
        .collect::<Result<_, _>>()?;
    let mut archive = conn.prepare(&format!(
        "UPDATE deliveries SET archived_at = ?1
         WHERE message_id = ?2 AND recipient_address_id = {} AND recipient_run_id = ''",
        address_sql(me.kind.into(), "?3")
    ))?;
    for m in &msgs {
        archive.execute(params![now, m.id, me.id])?;
    }
    Ok(msgs)
}

/// [`take_unread`]'s query: ?1 and ?2 the reader's kind and id, ?3 the limit.
fn take_unread_sql(kind: Kind) -> String {
    // A drain's final notice must follow older mail even if the wall clock
    // moved backwards. Later owner Cancel commands bypass the watermark;
    // the service validates their ownership before insertion. Preserve
    // timestamp ordering outside a restart barrier.
    format!(
        "{} AND d.archived_at IS NULL AND {}
         AND (?1 != 'agent' OR m.control = 'cancel' OR m.id <= coalesce((
           SELECT r.watermark FROM agent_drains r JOIN agent_owners o ON o.human_id = r.human_id
           WHERE o.agent_id = ?2), 9223372036854775807))
         ORDER BY CASE WHEN ?1 = 'agent' AND EXISTS (
           SELECT 1 FROM agent_drains r JOIN agent_owners o ON o.human_id = r.human_id
           WHERE o.agent_id = ?2) THEN m.id END, m.created_at, m.id LIMIT ?3",
        inbox_select(kind),
        *FOCUS_ALLOWS,
    )
}

pub fn archived_since(
    conn: &Connection,
    me: &Principal,
    since: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{} AND d.archived_at IS NOT NULL AND m.control IS NULL
         AND (?3 IS NULL OR m.created_at >= ?3)
         ORDER BY m.created_at, m.id LIMIT ?4",
        inbox_select(me.kind)
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
    /// Include broadcasts delivered to agents in `with`, at send time.
    pub include_group_deliveries: bool,
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
    /// Exclusive descending cursor for message search.
    pub before_id: Option<i64>,
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

/// For each of `messages`, the agents this human owns, live or retired, whose
/// conversation holds it: the sender, or a recipient when it was sent. Each
/// agent once, by id, with its current name; messages with none are absent.
pub fn owned_agent_scopes(
    conn: &Connection,
    human: i64,
    messages: &[i64],
) -> Result<HashMap<i64, Vec<(i64, String)>>> {
    let mut scopes: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    if messages.is_empty() {
        return Ok(scopes);
    }
    let ids = vec!["?"; messages.len()].join(", ");
    let sql = format!(
        "SELECT m.id, a.id, a.name FROM messages m
         JOIN entity_addresses sa ON sa.id = m.sender_address_id
         JOIN entity_local_bindings sb ON sb.entity_id = sa.entity_id
         JOIN agent_owners o ON o.agent_id = sb.agent_id AND o.human_id = ?1
         JOIN agents a ON a.id = sb.agent_id
         WHERE m.id IN ({ids})
         UNION
         SELECT d.message_id, a.id, a.name FROM deliveries d
         JOIN entity_addresses ra ON ra.id = d.recipient_address_id
         JOIN entity_local_bindings rb ON rb.entity_id = ra.entity_id
         JOIN agent_owners o ON o.agent_id = rb.agent_id AND o.human_id = ?1
         JOIN agents a ON a.id = rb.agent_id
         WHERE d.message_id IN ({ids})
         ORDER BY 1, 2"
    );
    let mut args = vec![rusqlite::types::Value::Integer(human)];
    for _ in 0..2 {
        args.extend(
            messages
                .iter()
                .map(|&id| rusqlite::types::Value::Integer(id)),
        );
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (message, agent, name) = row?;
        scopes.entry(message).or_default().push((agent, name));
    }
    Ok(scopes)
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
                    "nb.{} IN ({})",
                    hub_model::binding_column(kind.into()),
                    list(&mut args, &mut ids)
                ));
            }
        }
        // A delivery notice is BBS's, not a send by its addressee.
        clauses.push(format!(
            "m.sender_address_id IN ({}) AND m.kind = 'application'",
            addresses_sql(&any.join(" OR "))
        ));
    }
    if let Some(human) = filter
        .from_human
        .filter(|_| expanded != Expanded::FromHuman)
    {
        clauses.push(format!(
            "m.sender_address_id IN ({}) AND m.kind = 'application'",
            addresses_sql(&format!(
                "nb.human_id = ? OR nb.agent_id IN ({OWNED_AGENTS})"
            ))
        ));
        args.extend([human; 2].map(Value::Integer));
    }
    if let Some(human) = filter.scope.filter(|_| expanded != Expanded::Scope) {
        clauses.push(format!(
            "(m.sender_address_id IN ({})
              OR m.id IN (SELECT message_id FROM deliveries
                          WHERE recipient_address_id IN ({}))
              OR (m.to_group_id IS NULL
                  AND EXISTS (SELECT 1 FROM entity_addresses sa
                      JOIN entity_local_bindings sb ON sb.entity_id = sa.entity_id
                      WHERE sa.id = m.sender_address_id AND sb.human_id IS NOT NULL)
                  AND m.id IN (SELECT message_id FROM deliveries
                               WHERE recipient_address_id = {})))",
            addresses_sql(&format!(
                "nb.human_id = ? OR nb.agent_id IN ({OWNED_AGENTS})"
            )),
            addresses_sql(&format!("nb.agent_id IN ({OWNED_AGENTS})")),
            address_sql(EntityKind::Human, "?"),
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
    if let Some(before) = filter.before_id {
        clauses.push("m.id < ?".into());
        args.push(Value::Integer(before));
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
        && filter.after_id.is_none()
        && filter.before_id.is_none();
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
                "SELECT m.id FROM messages m
                 WHERE m.sender_address_id = {} AND m.kind = 'application'{predicates}
                 ORDER BY m.id {dir} LIMIT ?",
                address_sql(kind.into(), "?")
            ),
            vec![Value::Integer(id)],
        ),
        Term::DeliveredTo(agent) => (
            format!(
                "SELECT m.id FROM deliveries d JOIN messages m ON m.id = d.message_id
                 WHERE d.recipient_address_id = {} AND d.recipient_run_id = ''{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?",
                address_sql(EntityKind::Agent, "?")
            ),
            vec![Value::Integer(agent)],
        ),
        Term::DirectToAgent(agent) => (
            format!(
                "SELECT m.id FROM deliveries d JOIN messages m ON m.id = d.message_id
                 WHERE d.recipient_address_id = {} AND d.recipient_run_id = ''
                   AND m.to_group_id IS NULL{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?",
                address_sql(EntityKind::Agent, "?")
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
                "SELECT m.id FROM deliveries d INDEXED BY deliveries_direct
                 JOIN messages m ON m.id = d.message_id
                 WHERE d.direct = 1 AND d.recipient_address_id = {}{predicates}
                 ORDER BY d.message_id {dir} LIMIT ?",
                address_sql(EntityKind::Human, "?")
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
                Kind::Agent if filter.include_group_deliveries => Term::DeliveredTo(id),
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
        "SELECT m.id, m.created_at, {SENDER_KIND}, {}, g.name,
                COALESCE(ta.name, th.name, {}), d.archived_at, m.body, owner.name,
                to_owner.name, m.auto_reply, {}
         FROM messages m
         {SENDER_JOINS}
         {COPY_JOINS}
         LEFT JOIN agent_owners sender ON sender.agent_id = fa.id
         LEFT JOIN humans owner ON owner.id = sender.human_id
         LEFT JOIN bbs_groups g ON g.id = m.to_group_id
         LEFT JOIN deliveries d ON m.to_group_id IS NULL AND d.message_id = m.id
           AND d.recipient_run_id = ''
         LEFT JOIN entity_addresses ra ON ra.id = d.recipient_address_id
         LEFT JOIN entity_local_bindings rb ON rb.entity_id = ra.entity_id
         LEFT JOIN agents ta ON ta.id = rb.agent_id
         LEFT JOIN humans th ON th.id = rb.human_id
         LEFT JOIN agent_owners recipient ON recipient.agent_id = ta.id
         LEFT JOIN humans to_owner ON to_owner.id = recipient.human_id
         -- A remote recipient has no delivery: name its address instead.
         LEFT JOIN entity_addresses xa ON m.to_group_id IS NULL AND d.message_id IS NULL
           AND xa.id = m.recipient_address_id
         LEFT JOIN entities xe ON xe.id = xa.entity_id
         WHERE m.id IN ({})
         ORDER BY m.id",
        *SENDER_NAME,
        remote_display_sql("xe", "xa", "m.recipient_run_id"),
        copy_columns(None),
        vec!["?"; ids.len()].join(", ")
    );
    let rows = conn
        .prepare(&sql)?
        .query_map(rusqlite::params_from_iter(ids), |r| {
            let from_kind: String = r.get(2)?;
            let view = MessageView {
                id: r.get(0)?,
                created_at: r.get(1)?,
                from_kind: if from_kind == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                from_remote: from_kind == "remote",
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
                original_from: None,
                group_notice: false,
                reported: false,
                in_reply_to: None,
            };
            copy_view(view, r, 11)
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
    pub target_id: i64,
    pub target: String,
    pub creator: String,
    pub creator_kind: Kind,
    pub creator_id: i64,
    pub body: String,
    pub repeat: Option<String>,
    pub next_run: String,
    pub created_at: String,
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
        "SELECT t.id, t.target_id, a.name,
                CASE t.creator_kind WHEN 'agent' THEN ca.name ELSE ch.name END,
                t.body, t.repeat, t.next_run, t.created_at, t.creator_kind, t.creator_id
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
                target_id: r.get(1)?,
                target: r.get(2)?,
                creator: r.get(3)?,
                body: r.get(4)?,
                repeat: r.get(5)?,
                next_run: r.get(6)?,
                created_at: r.get(7)?,
                creator_kind: if r.get::<_, String>(8)? == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                creator_id: r.get(9)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// A timer's target, for checking who may manage it.
pub fn timer_target(conn: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT target_id FROM timers WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?)
}

/// One timer by id.
pub fn timer(conn: &Connection, id: i64) -> Result<Option<TimerRow>> {
    let Some(target_id) = timer_target(conn, id)? else {
        return Ok(None);
    };
    Ok(timers(conn, Some(target_id))?
        .into_iter()
        .find(|t| t.id == id))
}

/// Make `creator` the one a timer's messages come from.
pub fn set_timer_creator(conn: &Connection, id: i64, creator: &Principal) -> Result<()> {
    conn.execute(
        "UPDATE timers SET creator_kind = ?2, creator_id = ?3 WHERE id = ?1",
        params![id, creator.kind.as_str(), creator.id],
    )?;
    Ok(())
}

/// Replace a timer's text and schedule.
pub fn update_timer(
    conn: &Connection,
    id: i64,
    body: &str,
    repeat: Option<&str>,
    next_run: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE timers SET body = ?2, repeat = ?3, next_run = ?4 WHERE id = ?1",
        params![id, body, repeat, next_run],
    )?;
    Ok(())
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

    fn human(conn: &Connection, name: &str) -> Principal {
        Principal {
            kind: Kind::Human,
            id: insert_human(conn, name, "t").unwrap(),
            name: name.into(),
        }
    }

    fn agent_err(conn: &Connection, owner: i64, name: &str) -> String {
        let a = NewAgent {
            name,
            token: &crate::auth::new_token(),
            owner_id: owner,
            status: None,
            pwd: "/",
            hostname: "h",
            now: "t",
        };
        format!("{:#}", insert_agent(conn, &a).unwrap_err())
    }

    fn agent(conn: &Connection, owner: i64, name: &str) -> Principal {
        let a = NewAgent {
            name,
            token: &crate::auth::new_token(),
            owner_id: owner,
            status: None,
            pwd: "/",
            hostname: "h",
            now: "t",
        };
        Principal {
            kind: Kind::Agent,
            id: insert_agent(conn, &a).unwrap(),
            name: name.into(),
        }
    }

    /// The operator's cutover of a legacy (schema 26) test board.
    fn cut_over(conn: &mut Connection) {
        migrate_to_hub_model(conn, "will").unwrap();
    }

    #[test]
    fn list_agents_reports_last_sent_message_and_activity() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let will = human(&conn, "will");
        let mateo = agent(&conn, will.id, "Mateo");
        let zed = agent(&conn, will.id, "Zed");
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
        insert_message(
            &conn,
            &mateo,
            To::Direct(Kind::Human, will.id),
            "hi",
            "2026-10-17T10:00:00Z",
        )
        .unwrap();
        post_activity(
            &conn,
            mateo.id,
            "2026-10-17T09:00:00Z",
            &[("text".into(), "e".into())],
        )
        .unwrap();
        assert_eq!(last_sent("Mateo").as_deref(), Some("2026-10-17T10:00:00Z"));
        // Zed only posts activity.
        post_activity(
            &conn,
            zed.id,
            "2026-10-17T11:00:00Z",
            &[("text".into(), "e".into())],
        )
        .unwrap();
        assert_eq!(last_sent("Zed").as_deref(), Some("2026-10-17T11:00:00Z"));
        // Trimming keeps the newest events, so the max survives it.
        let events = |n: i64| -> Vec<(String, String)> {
            (0..n).map(|i| ("text".into(), format!("e{i}"))).collect()
        };
        post_activity(
            &conn,
            zed.id,
            "2026-10-17T12:00:00Z",
            &events(ACTIVITY_PER_AGENT),
        )
        .unwrap();
        post_activity(&conn, zed.id, "2026-10-17T13:00:00Z", &events(5)).unwrap();
        assert_eq!(last_sent("Zed").as_deref(), Some("2026-10-17T13:00:00Z"));
    }

    #[test]
    fn last_sent_lookups_do_not_grow_with_history() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let will = human(&conn, "will");
        let mateo = agent(&conn, will.id, "Mateo");
        let fill = |conn: &Connection, n: usize| {
            let mut stmt = conn
                .prepare(&format!(
                    "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
                         recipient_address_id, body, created_at, authored_at)
                     VALUES ('bbs', 'fill-' || coalesce((SELECT max(id) FROM messages), 0), {}, {}, 'x', 't', 't')",
                    address_sql(EntityKind::Agent, "?1"),
                    address_sql(EntityKind::Human, "?2"),
                ))
                .unwrap();
            for _ in 0..n {
                stmt.execute([mateo.id, will.id]).unwrap();
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
    fn a_new_generation_and_retirement_clear_unused_side_question_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (1, 'Mateo', 'a', '/', 'h', 't', 't');
             INSERT INTO side_state (agent_id, generation, reported_at, revision, closed_seq,
               supported, reason, followups)
             VALUES (1, 'run-1', 't', 1, 0, 1, 'r', 1);
             INSERT INTO side_requests (agent_id, generation, question, asked_at, status, updated_at)
             VALUES (1, 'run-1', 'q?', 't', 'open', 't')",
        )
        .unwrap();
        assert!(begin_side_session(&conn, 1, "run-2", "t2").unwrap());
        let state: (Option<i64>, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT supported, reason, followups FROM side_state WHERE agent_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (None, None, None));
        let status: String = conn
            .query_row("SELECT status FROM side_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "interrupted");
        retire_agent(&conn, 1, "t3").unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM side_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
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
        migrate_legacy(&mut conn).unwrap();
        migrate_legacy(&mut conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), HUB_MODEL_SOURCE_VERSION);
        cut_over(&mut conn);
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
        // The archived group keeps its entity ID, so its name is refused.
        assert_eq!(
            native_address(&conn, EntityKind::Group, 1)
                .unwrap()
                .unwrap()
                .entity
                .entity_id,
            "acme"
        );
        assert_eq!(
            format!(
                "{:#}",
                insert_group(&conn, "acme", None, "2026-10-15").unwrap_err()
            ),
            "group name acme is unavailable: acme belongs to an archived group; \
             Hub IDs are never reused. Choose another name."
        );
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
        migrate_legacy(&mut conn).unwrap();
        migrate_legacy(&mut conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), HUB_MODEL_SOURCE_VERSION);
        cut_over(&mut conn);
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
            migrate_legacy(&mut conn)
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
                "deliveries_direct",
            ),
            (
                term_sql(Term::DirectToAgent(2), 200, &mine, Expanded::With),
                "deliveries_recipient",
            ),
            (
                term_sql(Term::DeliveredTo(2), 200, &mine, Expanded::With),
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
            .prepare(&format!("EXPLAIN QUERY PLAN {}", *AGENT_SUMMARIES))
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
                .any(|l| l.contains("messages_sender (sender_address_id=?)")),
            "{plan:#?}"
        );
        assert!(
            plan.iter()
                .any(|l| l.starts_with("SEARCH activity USING INDEX activity_agent (agent_id=?)")),
            "{plan:#?}"
        );
        // So does the turn-event lookup.
        assert!(
            plan.iter()
                .any(|l| l.starts_with("SEARCH turn USING INDEX activity_agent (agent_id=?)")),
            "{plan:#?}"
        );
    }

    #[test]
    fn turn_event_is_the_newest_event_that_starts_or_ends_a_turn() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO humans (id, name, created_at, active_at) VALUES (1, 'will', 't', 't');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
               VALUES (1, 'Theo', 'h1', '/', 'host', 't', 't');",
        )
        .unwrap();
        let turn = |conn: &Connection| {
            let listed = list_agents(conn, false).unwrap()[0].turn_event.clone();
            let summed = agent_summaries(conn, 1).unwrap()[0].turn_event.clone();
            assert_eq!(listed, summed);
            listed
        };
        assert_eq!(turn(&conn), None);
        let post = |conn: &Connection, events: &[&str]| {
            let rows: Vec<_> = events
                .iter()
                .map(|e| (e.to_string(), format!(r#"{{"event":"{e}"}}"#)))
                .collect();
            post_activity(conn, 1, "2026-11-09T00:00:00Z", &rows).unwrap();
        };
        post(&conn, &["mail", "prompt", "tool", "text"]);
        assert_eq!(turn(&conn).as_deref(), Some("text"));
        // Events after a turn ends, such as a goal report, don't count.
        post(&conn, &["turn_end", "goal", "warning"]);
        assert_eq!(turn(&conn).as_deref(), Some("turn_end"));
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
                        (3, 'Mateo', 't3', '/', 'h', 'x', 'x', '2026-10-18T00:00:00.000000Z'),
                        (4, 'Mateo', 't4', '/', 'h', 'x', 'x', NULL);
                 INSERT INTO agent_owners VALUES (1, 1), (2, 1), (3, 1), (4, 1);
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
        migrate_legacy(&mut old).unwrap();

        // Incremental: the same history recorded as it is sent.
        let mut new = Connection::open_in_memory().unwrap();
        migrate_legacy(&mut new).unwrap();
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
        cut_over(&mut old);
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
            migrate_legacy(&mut conn).unwrap();
            migrate_legacy(&mut conn).unwrap();
            assert_eq!(schema_version(&conn).unwrap(), HUB_MODEL_SOURCE_VERSION);
            assert_eq!(index_names(&conn).len(), 5, "preexisting: {preexisting}");
            cut_over(&mut conn);
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
        migrate_legacy(&mut conn).unwrap();
        let direct: Vec<(i64, bool)> = conn
            .prepare("SELECT message_id, direct FROM deliveries ORDER BY message_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(direct, [(1, true), (2, false)]);
        cut_over(&mut conn);
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
        let to_bram = To::Direct(Kind::Agent, bram.id);
        let ordinary = insert_message(&conn, &will, to_bram, "hi Bram", t).unwrap();
        // Controls straddle the ordinary message in time: one older (pins
        // archived history's oldest-first limit), one newer (pins the feed's
        // newest-first limit). A post-LIMIT filter would fail either window.
        let cancel = insert_message(
            &conn,
            &will,
            to_bram,
            "cancel",
            "2026-10-08T23:59:58.000000Z",
        )
        .unwrap();
        set_message_control(&conn, cancel, "cancel").unwrap();
        let effort = insert_message(
            &conn,
            &will,
            to_bram,
            "set-effort",
            "2026-10-09T00:00:02.000000Z",
        )
        .unwrap();
        set_message_control(&conn, effort, "set-effort").unwrap();
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
            FeedFilter {
                with: Some(vec![(Kind::Agent, bram.id)]),
                include_group_deliveries: true,
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
        assert!(
            effort_control_body(&conn, bram.id, ordinary)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            effort_control_body(&conn, bram.id, effort)
                .unwrap()
                .as_deref(),
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

        let m = insert_message(&conn, &a, To::Group(g), "hello all", t).unwrap();
        for p in [&b, &c, &will] {
            insert_delivery(&conn, m, p.kind, p.id).unwrap();
        }
        insert_message(&conn, &will, To::Direct(Kind::Agent, b.id), "hi Bram", t).unwrap();

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

    #[test]
    fn new_database_is_created_at_the_hub_model_even_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bbs.db");
        // A bad namespace creates nothing.
        let mut conn = open(&path).unwrap();
        assert!(migrate_in(&mut conn, Some("Bad_NS")).is_err());
        assert_eq!(schema_version(&conn).unwrap(), 0);
        drop(conn);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut conn = open(&path).unwrap();
                    migrate_in(&mut conn, Some("acme")).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(namespace(&conn).unwrap(), "acme");
        let rows: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM entities) + (SELECT count(*) FROM messages)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        migrate(&mut conn).unwrap();
        migrate_in(&mut conn, Some("acme")).unwrap();
        assert!(migrate_in(&mut conn, Some("other")).is_err());
        assert!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                .unwrap()
        );

        // Without a namespace, a new database gets the default.
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(namespace(&conn).unwrap(), DEFAULT_NAMESPACE);
    }

    #[test]
    fn new_records_get_entity_ids_by_the_cutover_rules() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_in(&mut conn, Some("work")).unwrap();
        let entity = |kind, id| {
            let address = native_address(&conn, kind, id).unwrap().unwrap();
            assert_eq!(address.entity.namespace, "work");
            assert_eq!(address.address_id, NATIVE_ADDRESS);
            let (lifecycle, display): (String, String) = conn
                .query_row(
                    "SELECT e.lifecycle, e.display_name FROM entities e
                     WHERE e.namespace = 'work' AND e.entity_id = ?1",
                    [&address.entity.entity_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            (address.entity.entity_id, lifecycle, display)
        };
        let id = |e: (String, String, String)| e.0;
        let ann = human(&conn, "ann.lee");
        assert_eq!(
            entity(EntityKind::Human, ann.id),
            ("ann-lee".into(), "active".into(), "ann.lee".into())
        );
        let first = agent(&conn, ann.id, "a.b");
        assert_eq!(id(entity(EntityKind::Agent, first.id)), "ann-lee/a-b");
        // A live agent's ID is refused, including after `.` maps to `-`.
        assert_eq!(
            agent_err(&conn, ann.id, "a-b"),
            "agent name a-b is unavailable: ann-lee/a-b belongs to an agent; \
             Hub IDs are never reused. Choose another name."
        );
        // Retiring keeps the ID, and it is never reused, in any case.
        retire_agent(&conn, first.id, "t").unwrap();
        assert_eq!(
            entity(EntityKind::Agent, first.id),
            ("ann-lee/a-b".into(), "retired".into(), "a.b".into())
        );
        for name in ["a.b", "A.B", "a-b"] {
            assert_eq!(
                agent_err(&conn, ann.id, name),
                format!(
                    "agent name {name} is unavailable: ann-lee/a-b belongs to a retired agent; \
                     Hub IDs are never reused. Choose another name."
                )
            );
        }
        // Another owner's agent of the same name has its own ID.
        let bo = human(&conn, "bo");
        let other = agent(&conn, bo.id, "a.b");
        assert_eq!(id(entity(EntityKind::Agent, other.id)), "bo/a-b");
        let retired: String = conn
            .query_row(
                &format!(
                    "SELECT lifecycle FROM entity_addresses WHERE id = {}",
                    address_sql(EntityKind::Agent, "?1")
                ),
                [first.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retired, "retired");
        // Humans and groups share the top level, so a group cannot take a
        // person's ID, and a person cannot take a group's.
        for name in ["ann-lee", "Ann.Lee"] {
            assert_eq!(
                format!("{:#}", insert_group(&conn, name, None, "t").unwrap_err()),
                format!(
                    "group name {name} is unavailable: ann-lee belongs to a human; \
                     Hub IDs are never reused. Choose another name."
                )
            );
        }
        let group = insert_group(&conn, "crew", None, "t").unwrap();
        assert_eq!(id(entity(EntityKind::Group, group)), "crew");
        assert!(
            format!("{:#}", insert_human(&conn, "CREW", "t").unwrap_err())
                .contains("crew belongs to a group;")
        );
        archive_group(&conn, group, "t").unwrap();
        assert_eq!(entity(EntityKind::Group, group).1, "retired");
        let handlers: Vec<String> = conn
            .prepare("SELECT handler FROM address_delivery_bindings ORDER BY address_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(handlers, ["inbox", "inbox", "inbox", "inbox", "fanout"]);
        assert!(
            !conn
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .exists([])
                .unwrap()
        );
    }

    #[test]
    fn agents_registered_after_the_cutover_never_take_a_migrated_id() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_legacy(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO humans VALUES (1, 'will', 't', 't');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
             VALUES (1, 'scout', 'k1', '/', 'h', 't', 't', '2026-10-08T00:00:00.000000Z'),
                    (2, 'scout', 'k2', '/', 'h', 't', 't', '2026-10-09T00:00:00.000000Z');
             INSERT INTO agent_owners VALUES (1, 1), (2, 1);",
        )
        .unwrap();
        cut_over(&mut conn);
        let ids = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT entity_id FROM entities WHERE kind = 'agent' ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(ids(&conn), ["will/scout-retired-2026-10-08", "will/scout"]);
        assert!(agent_err(&conn, 1, "scout").contains("will/scout belongs to a retired agent;"));
        assert!(
            agent_err(&conn, 1, "scout-retired-2026-10-08")
                .contains("will/scout-retired-2026-10-08 belongs to a retired agent;")
        );
        agent(&conn, 1, "scout2");
        assert_eq!(
            ids(&conn),
            ["will/scout-retired-2026-10-08", "will/scout", "will/scout2"]
        );
    }

    #[test]
    fn messages_refer_to_native_addresses_with_their_own_origin_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_in(&mut conn, Some("work")).unwrap();
        let will = human(&conn, "will");
        let scout = agent(&conn, will.id, "scout");
        let g = insert_group(&conn, "crew", None, "t").unwrap();
        let direct =
            insert_message(&conn, &will, To::Direct(Kind::Agent, scout.id), "hi", "t").unwrap();
        let broadcast = insert_message(&conn, &scout, To::Group(g), "all", "t").unwrap();
        insert_delivery(&conn, broadcast, Kind::Human, will.id).unwrap();
        let address = |kind, id| -> i64 {
            conn.query_row(&format!("SELECT {}", address_sql(kind, "?1")), [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let row = |id: i64| -> (String, String, i64, i64, Option<i64>) {
            conn.query_row(
                "SELECT origin_namespace, origin_message_id, sender_address_id,
                        recipient_address_id, to_group_id FROM messages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };
        let (namespace, origin, from, to, group) = row(direct);
        assert_eq!(namespace, "work");
        assert!(origin.len() >= 26 && !origin.starts_with("bbs-legacy-"));
        assert_eq!(
            (from, to, group),
            (
                address(EntityKind::Human, will.id),
                address(EntityKind::Agent, scout.id),
                None
            )
        );
        let (_, other, from, to, group) = row(broadcast);
        assert_ne!(other, origin);
        assert_eq!(
            (from, to, group),
            (
                address(EntityKind::Agent, scout.id),
                address(EntityKind::Group, g),
                Some(g)
            )
        );
        let deliveries: Vec<(i64, i64, bool)> = conn
            .prepare("SELECT message_id, recipient_address_id, direct FROM deliveries ORDER BY 1")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            deliveries,
            [
                (direct, address(EntityKind::Agent, scout.id), true),
                (broadcast, address(EntityKind::Human, will.id), false)
            ]
        );
        // Read state round-trips through the delivery.
        assert!(has_unread(&conn, &scout).unwrap());
        let read = take_unread(&conn, &scout, None, "t2").unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(
            (read[0].from_kind, read[0].from_name.as_str()),
            (Kind::Human, "will")
        );
        assert!(!has_unread(&conn, &scout).unwrap());
        assert_eq!(archived_since(&conn, &scout, None, None).unwrap().len(), 1);
        assert_eq!(mark_read_from(&conn, &will, &scout, "t3").unwrap(), 1);
    }

    #[test]
    fn a_refused_legacy_database_is_left_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bbs.db");
        let mut conn = Connection::open(&path).unwrap();
        migrate_legacy(&mut conn).unwrap();
        conn.execute_batch("INSERT INTO humans VALUES (1, 'will', 't', 't')")
            .unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        let mut conn = open(&path).unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        drop(conn);
        // It says exactly what to run on this file.
        let db = path.display().to_string();
        let state = dir.path().display().to_string();
        for expected in [
            format!("{db} is at BBS schema v26, before the Hub model (v27)"),
            format!("bbs backup {state}/pre-hub.db --state-dir {state}"),
            format!(
                "cargo install --locked bbs-server --version {}",
                crate::release::VERSION
            ),
            format!(
                "bbs-server migrate-hub-model --db {db} --namespace bbs --backup-taken {state}/pre-hub.db"
            ),
            "keep `bbs`, the default for new databases".into(),
        ] {
            assert!(err.contains(&expected), "{expected}\n{err}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, ["bbs.db"]);
    }

    #[test]
    fn a_hub_model_database_without_its_namespace_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch("DROP TABLE bbs_installation").unwrap();
        for namespace in [None, Some("bbs")] {
            let err = migrate_in(&mut conn, namespace).unwrap_err().to_string();
            assert!(
                err.contains("has no installation namespace (bbs_installation)")
                    && err.contains("migrate it again with this release"),
                "{err}"
            );
        }
    }

    /// The query plan of `sql`, every parameter bound to 1.
    fn plan_of(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = stmt.parameter_count();
        stmt.query_map(
            rusqlite::params_from_iter(vec![rusqlite::types::Value::Integer(1); n]),
            |r| r.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    #[test]
    fn inbox_queries_walk_the_recipients_inbox_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let inbox =
            "deliveries_inbox (recipient_address_id=? AND recipient_run_id=? AND archived_at=?";
        let mut cases = Vec::new();
        for kind in [Kind::Agent, Kind::Human] {
            cases.push(("take_unread", take_unread_sql(kind)));
            cases.push(("has_unread", has_unread_sql(kind)));
            for from in [Kind::Agent, Kind::Human] {
                cases.push(("mark_read_from", mark_read_from_sql(kind, from)));
            }
        }
        cases.push(("agent_summaries", AGENT_SUMMARIES.to_string()));
        for (name, sql) in cases {
            let plan = plan_of(&conn, &sql);
            assert!(plan.iter().any(|l| l.contains(inbox)), "{name}: {plan:#?}");
            assert!(
                !plan.iter().any(|l| l.starts_with("SCAN d")
                    || l.starts_with("SCAN deliveries")
                    || l.starts_with("SCAN m")
                    || l.starts_with("SCAN messages")),
                "{name}: {plan:#?}"
            );
            if name == "mark_read_from" {
                assert!(
                    plan.iter()
                        .any(|l| l.contains("messages_sender (sender_address_id=?)")),
                    "{name}: {plan:#?}"
                );
            }
        }
    }
}
