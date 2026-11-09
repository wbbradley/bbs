//! The Hub model (schema 27) and the cutover to it from schema 26.
//!
//! A new database is created at schema 27 (see `db::migrate`). An existing
//! one moves only when an operator runs the cutover; startup never does. The
//! cutover moves messages and deliveries onto Hub's entity/address model and
//! gives every human, agent and group one entity ID, computed here from its
//! BBS name (BBS blocker 1396):
//!
//! - `.` becomes `-`; any other character outside `[A-Za-z0-9_-]` blocks.
//! - Humans, groups and ownerless agents get a top-level ID. They share one
//!   space, compared case-insensitively; any clash blocks.
//! - An owned agent gets `owner/name`. Within one owner, different names that
//!   map to one ID (`a.b`, `a-b`) block. A reused name keeps the plain ID for
//!   the live agent (else the most recently retired one), and every other
//!   agent gets `owner/name-retired-YYYY-MM-DD` (BBS blocker 1407; see
//!   `map_owner_scope` and `assign_dated`).
//!
//! Records created after the cutover get IDs from the same name mapping (see
//! [`create_entity`]). IDs never change, so one whose ID is taken gets a
//! counter instead of blocking.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, TransactionBehavior, params};

use super::{MIGRATIONS, is_busy, schema_version};
use crate::entity::{EntityKind, NATIVE_ADDRESS};

/// The only schema the cutover starts from.
pub const HUB_MODEL_SOURCE_VERSION: usize = 26;
/// The schema the cutover produces.
pub const HUB_MODEL_SCHEMA_VERSION: usize = HUB_MODEL_SOURCE_VERSION + 1;

// A regular migration must not take the cutover's schema number: adding one
// to MIGRATIONS breaks the build until the cutover is retargeted.
const _: () = assert!(
    MIGRATIONS.len() == HUB_MODEL_SOURCE_VERSION,
    "retarget the Hub model migration to the new MIGRATIONS list"
);
const _: () = assert!(MIGRATIONS.len() < HUB_MODEL_SCHEMA_VERSION);

/// How long the cutover waits for other connections before refusing.
const LOCK_WAIT: Duration = Duration::from_millis(200);

const HUB_MODEL_SQL: &str = include_str!("../../migrations/0027_hub_model.sql");
const MAX_ENTITY_ID: usize = 128;
/// How many offending records one preflight issue lists.
const MAX_EXAMPLES: usize = 20;

/// One native record and the entity ID the cutover gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityMapping {
    pub kind: EntityKind,
    pub native_id: i64,
    /// The BBS name, kept as the entity's display name.
    pub name: String,
    pub entity_id: String,
    /// A retired agent or an archived group.
    pub retired: bool,
}

impl EntityMapping {
    /// Whether the ID's name part differs from the BBS name: a `.` became `-`
    /// or a retired agent got a dated ID.
    pub fn renamed(&self) -> bool {
        self.entity_id.rsplit('/').next() != Some(self.name.as_str())
    }
}

/// What the cutover would do, computed without writing anything.
#[derive(Debug)]
pub struct PreflightReport {
    pub namespace: String,
    /// Every human, agent and group, ordered by kind and native ID.
    pub mappings: Vec<EntityMapping>,
    pub messages: i64,
    pub deliveries: i64,
    /// Blocking problems; the cutover refuses unless this is empty.
    pub issues: Vec<String>,
}

impl PreflightReport {
    pub fn is_blocked(&self) -> bool {
        !self.issues.is_empty()
    }

    pub fn count(&self, kind: EntityKind) -> usize {
        self.mappings.iter().filter(|m| m.kind == kind).count()
    }

    pub fn renamed(&self) -> Vec<EntityMapping> {
        self.mappings
            .iter()
            .filter(|m| m.renamed())
            .cloned()
            .collect()
    }
}

/// What a completed cutover migrated.
#[derive(Debug)]
pub struct HubMigrationReport {
    pub humans: usize,
    pub agents: usize,
    pub groups: usize,
    pub messages: i64,
    pub deliveries: i64,
    /// Mappings that needed `.` to `-` or a dated ID.
    pub renamed: Vec<EntityMapping>,
}

/// Hub's namespace grammar: 1-63 characters, `[a-z][a-z0-9-]*`.
pub fn validate_namespace(namespace: &str) -> Result<()> {
    let mut chars = namespace.chars();
    let valid = (1..=63).contains(&namespace.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        bail!("invalid namespace {namespace:?}: expected 1-63 characters matching [a-z][a-z0-9-]*");
    }
    Ok(())
}

fn require_source_version(conn: &Connection) -> Result<()> {
    let version = schema_version(conn)?;
    if version != HUB_MODEL_SOURCE_VERSION {
        bail!(
            "the Hub model migration needs BBS schema v{HUB_MODEL_SOURCE_VERSION}; \
             this database is at v{version}"
        );
    }
    Ok(())
}

/// Compute the cutover's mapping and every blocking issue without writing.
pub fn hub_model_preflight(conn: &Connection, namespace: &str) -> Result<PreflightReport> {
    validate_namespace(namespace)?;
    if conn.is_autocommit() {
        // One read snapshot; dropping the transaction rolls it back.
        let tx = conn.unchecked_transaction()?;
        preflight(&tx, namespace)
    } else {
        preflight(conn, namespace)
    }
}

/// Take an exclusive lock on the database and keep it until `conn` closes,
/// refusing quickly if any other connection uses the database.
///
/// In WAL mode every connection that has read or written keeps the WAL index
/// open, which prevents the exclusive lock even while that connection is idle.
/// A connection opened afterwards cannot read until this one closes. In a
/// rollback-journal database, idle connections hold no lock, so only active
/// ones are detected; bbs-server always uses WAL.
pub fn lock_exclusive(conn: &Connection) -> Result<()> {
    if !conn.is_autocommit() {
        bail!("locking the database requires a connection without an active transaction");
    }
    let previous: i64 = conn.pragma_query_value(None, "busy_timeout", |r| r.get(0))?;
    conn.busy_timeout(LOCK_WAIT)?;
    let _: String =
        conn.pragma_update_and_check(None, "locking_mode", "EXCLUSIVE", |r| r.get(0))?;
    // In EXCLUSIVE locking mode the lock taken here outlives the transaction.
    let result = conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;");
    conn.busy_timeout(Duration::from_millis(previous.max(0) as u64))?;
    let Err(err) = result else { return Ok(()) };
    if !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    let _: rusqlite::Result<String> =
        conn.pragma_update_and_check(None, "locking_mode", "NORMAL", |r| r.get(0));
    if is_busy(&err) {
        bail!(
            "another connection is using this database; stop bbs-server and every other \
             bbs process using it first"
        );
    }
    Err(err).context("locking the database")
}

/// Run the cutover from schema 26 to 27 in one transaction. Refuses, leaving
/// the database unchanged at schema 26, unless it gets exclusive access (see
/// [`lock_exclusive`]; the lock lasts until `conn` closes), and on any
/// preflight issue or failed check.
pub fn migrate_to_hub_model(conn: &mut Connection, namespace: &str) -> Result<HubMigrationReport> {
    validate_namespace(namespace)?;
    if !conn.is_autocommit() {
        bail!("the Hub model migration requires a connection without an active transaction");
    }
    lock_exclusive(conn)?;
    require_source_version(conn)?;
    // As in `migrate`: SQLite ignores this pragma inside a transaction.
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> Result<HubMigrationReport> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let preflight = preflight(&tx, namespace)?;
        if preflight.is_blocked() {
            bail!(
                "Hub model preflight found blocking issues; nothing was changed:\n  - {}",
                preflight.issues.join("\n  - ")
            );
        }
        let report = apply(&tx, &preflight)?;
        tx.commit()?;
        Ok(report)
    })();
    conn.pragma_update(None, "foreign_keys", foreign_keys)?;
    result
}

/// Bring a new, empty database from schema 26 to 27 in `namespace`, inside
/// the caller's transaction: `db::migrate` applies 1-26, sets schema 26, then
/// calls this.
pub(super) fn create_in(tx: &Connection, namespace: &str) -> Result<()> {
    validate_namespace(namespace)?;
    let preflight = preflight(tx, namespace)?;
    if preflight.is_blocked() || !preflight.mappings.is_empty() || preflight.messages > 0 {
        bail!("a new database must be empty before it reaches the Hub model");
    }
    apply(tx, &preflight)?;
    Ok(())
}

/// The installation's namespace (schema 27).
pub fn namespace(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT namespace FROM bbs_installation", [], |r| r.get(0))?)
}

/// Give a native record created at schema 27 its entity, native address and
/// delivery handler, with an ID from the cutover's name mapping: `owner/name`
/// under its owner's entity ID for an owned agent, else top level. IDs are
/// immutable, so a reused or clashing name (an agent named like its retired
/// predecessor, a group named like a human) takes the lowest free `-N`, N
/// from 2, compared case-insensitively. Returns the entity ID.
pub(crate) fn create_entity(
    conn: &Connection,
    kind: EntityKind,
    native_id: i64,
    name: &str,
    owner: Option<i64>,
    now: &str,
) -> Result<String> {
    let namespace = namespace(conn)?;
    let segment = map_name(name);
    if !valid_segment(&segment) {
        bail!(
            "{} name {name:?} maps to `{segment}`, which is not a valid Hub ID segment",
            kind.as_str()
        );
    }
    let base = match owner {
        Some(owner) => {
            let parent: String = conn
                .query_row(
                    "SELECT e.entity_id FROM entities e
                     JOIN entity_local_bindings b ON b.entity_id = e.id WHERE b.human_id = ?1",
                    [owner],
                    |r| r.get(0),
                )
                .with_context(|| format!("no entity for human {owner}"))?;
            format!("{parent}/{segment}")
        }
        None => segment,
    };
    // LIKE ignores ASCII case; `_` matching any character only widens the set.
    let mut stmt = conn.prepare_cached(
        "SELECT lower(entity_id) FROM entities WHERE namespace = ?1 AND entity_id LIKE ?2 || '%'",
    )?;
    let taken: HashSet<String> = stmt
        .query_map(params![namespace, base], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let entity_id = std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|id| !taken.contains(&id.to_lowercase()))
        .expect("an unbounded range finds a free counter");
    if entity_id.len() > MAX_ENTITY_ID {
        bail!("entity ID `{entity_id}` is longer than {MAX_ENTITY_ID} characters");
    }
    conn.execute(
        "INSERT INTO entities (namespace, entity_id, kind, display_name, lifecycle, created_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
        params![namespace, entity_id, kind.as_str(), name, now],
    )?;
    let entity = conn.last_insert_rowid();
    let column = binding_column(kind);
    conn.execute(
        &format!("INSERT INTO entity_local_bindings (entity_id, {column}) VALUES (?1, ?2)"),
        params![entity, native_id],
    )?;
    conn.execute(
        "INSERT INTO entity_addresses (entity_id, address_id, label, lifecycle, is_default)
         VALUES (?1, ?2, 'BBS', 'active', 1)",
        params![entity, NATIVE_ADDRESS],
    )?;
    let handler = match kind {
        EntityKind::Group => "fanout",
        EntityKind::Human | EntityKind::Agent => "inbox",
    };
    conn.execute(
        "INSERT INTO address_delivery_bindings (address_id, handler) VALUES (?1, ?2)",
        params![conn.last_insert_rowid(), handler],
    )?;
    Ok(entity_id)
}

/// The `entity_local_bindings` column holding a native record of `kind`.
pub(crate) fn binding_column(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Human => "human_id",
        EntityKind::Agent => "agent_id",
        EntityKind::Group => "group_id",
    }
}

/// Mark a retired agent's or archived group's entity and addresses retired.
pub(crate) fn retire_entity(conn: &Connection, kind: EntityKind, native_id: i64) -> Result<()> {
    let column = binding_column(kind);
    let entity = format!("(SELECT entity_id FROM entity_local_bindings WHERE {column} = ?1)");
    conn.execute(
        &format!("UPDATE entities SET lifecycle = 'retired' WHERE id = {entity}"),
        [native_id],
    )?;
    conn.execute(
        &format!("UPDATE entity_addresses SET lifecycle = 'retired' WHERE entity_id = {entity}"),
        [native_id],
    )?;
    Ok(())
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
}

fn strings(conn: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn examples(items: &[String]) -> String {
    let mut out = items
        .iter()
        .take(MAX_EXAMPLES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > MAX_EXAMPLES {
        out.push_str(&format!(", ... ({} in all)", items.len()));
    }
    out
}

fn preflight(conn: &Connection, namespace: &str) -> Result<PreflightReport> {
    require_source_version(conn)?;
    let mut issues = Vec::new();
    let integrity = strings(conn, "PRAGMA integrity_check")?;
    if integrity != ["ok"] {
        issues.push(format!(
            "PRAGMA integrity_check failed: {}",
            examples(&integrity)
        ));
    }
    let foreign = strings(
        conn,
        "SELECT \"table\" || ' row ' || ifnull(rowid, '?') || ' -> ' || parent
         FROM pragma_foreign_key_check",
    )?;
    if !foreign.is_empty() {
        issues.push(format!(
            "{} invalid foreign-key references: {}",
            foreign.len(),
            examples(&foreign)
        ));
    }
    let mappings = map_entities(conn, &mut issues)?;
    history_issues(conn, &mut issues)?;
    Ok(PreflightReport {
        namespace: namespace.to_owned(),
        mappings,
        messages: count(conn, "messages")?,
        deliveries: count(conn, "deliveries")?,
        issues,
    })
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn describe(kind: EntityKind, id: i64, name: &str) -> String {
    format!("{} {id} `{name}`", kind.as_str())
}

/// Report a mapped name segment or a whole ID outside Hub's grammar.
fn check_id(m: &EntityMapping, segment: &str, issues: &mut Vec<String>) {
    let who = describe(m.kind, m.native_id, &m.name);
    if !valid_segment(segment) {
        issues.push(format!(
            "{who} maps to `{segment}`, which is not a valid Hub ID segment ([A-Za-z0-9_-]+)"
        ));
    } else if m.entity_id.len() > MAX_ENTITY_ID {
        issues.push(format!(
            "{who} maps to `{}`, longer than {MAX_ENTITY_ID} characters",
            m.entity_id
        ));
    }
}

fn map_name(name: &str) -> String {
    name.replace('.', "-")
}

fn map_entities(conn: &Connection, issues: &mut Vec<String>) -> Result<Vec<EntityMapping>> {
    let mut mappings = Vec::new();
    let mut top_level = Vec::new();
    let mut owner_names = HashMap::new();
    let mut stmt = conn.prepare("SELECT id, name FROM humans ORDER BY id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        owner_names.insert(id, map_name(&name));
        top_level.push((EntityKind::Human, id, name, false));
    }
    let mut stmt =
        conn.prepare("SELECT id, name, archived_at IS NOT NULL FROM bbs_groups ORDER BY id")?;
    for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
        let (id, name, archived) = row?;
        top_level.push((EntityKind::Group, id, name, archived));
    }
    let mut owned: BTreeMap<i64, Vec<Agent>> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.retired_at, o.human_id
         FROM agents a LEFT JOIN agent_owners o ON o.agent_id = a.id ORDER BY a.id",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?))
    })? {
        let (id, name, retired_at, owner): (i64, String, Option<String>, _) = row?;
        match owner {
            Some(owner) => owned.entry(owner).or_default().push(Agent {
                id,
                name,
                retired_at,
            }),
            None => top_level.push((EntityKind::Agent, id, name, retired_at.is_some())),
        }
    }

    // Top level: no suffixes; every clash blocks.
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (kind, id, name, retired) in top_level {
        let entity_id = map_name(&name);
        let m = EntityMapping {
            kind,
            native_id: id,
            name,
            entity_id: entity_id.clone(),
            retired,
        };
        check_id(&m, &entity_id, issues);
        by_id
            .entry(entity_id.to_lowercase())
            .or_default()
            .push(describe(kind, id, &m.name));
        mappings.push(m);
    }
    for (entity_id, records) in &by_id {
        if records.len() > 1 {
            issues.push(format!(
                "top-level ID `{entity_id}` (case-insensitive) is claimed by {}",
                records.join(", ")
            ));
        }
    }

    // Owned agents: per owner scope (see `map_owner_scope`).
    for (owner, agents) in owned {
        let Some(owner_name) = owner_names.get(&owner) else {
            // Also a foreign-key issue; no scope to place these agents in.
            issues.push(format!("agents owned by missing human {owner}"));
            continue;
        };
        for (agent, segment) in map_owner_scope(owner_name, agents, issues) {
            let m = EntityMapping {
                kind: EntityKind::Agent,
                native_id: agent.id,
                entity_id: format!("{owner_name}/{segment}"),
                retired: agent.retired_at.is_some(),
                name: agent.name,
            };
            check_id(&m, &segment, issues);
            mappings.push(m);
        }
    }
    mappings.sort_by_key(|m| (m.kind, m.native_id));
    Ok(mappings)
}

/// An owned agent as the mapping reads it.
struct Agent {
    id: i64,
    name: String,
    retired_at: Option<String>,
}

/// A retired agent that needs a dated ID, and the fallback level it reached.
struct Dated {
    index: usize,
    base: String,
    retired_at: DateTime<Utc>,
    level: DateLevel,
}

/// Dated-ID fallbacks, in the order they are tried (BBS blocker 1407).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DateLevel {
    /// `name-retired-YYYY-MM-DD`
    Day,
    /// `name-retired-YYYY-MM-DD-HHMM`
    Minute,
    /// `name-retired-YYYY-MM-DD-HHMMSS`
    Second,
    /// `name-retired-YYYY-MM-DD-HHMMSS-N`, N from 2, in agents.id order.
    Counter,
}

impl DateLevel {
    fn next(self) -> DateLevel {
        match self {
            DateLevel::Day => DateLevel::Minute,
            DateLevel::Minute => DateLevel::Second,
            DateLevel::Second | DateLevel::Counter => DateLevel::Counter,
        }
    }
}

impl Dated {
    /// The ID at this level; the Counter level's ID before its counter.
    fn segment(&self) -> String {
        let format = match self.level {
            DateLevel::Day => "%Y-%m-%d",
            DateLevel::Minute => "%Y-%m-%d-%H%M",
            DateLevel::Second | DateLevel::Counter => "%Y-%m-%d-%H%M%S",
        };
        format!("{}-retired-{}", self.base, self.retired_at.format(format))
    }
}

/// `retired_at` as BBS writes it: RFC3339 (UTC, microseconds), read as UTC.
fn parse_retired_at(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Entity-ID name segments for one owner's agents (BBS blocker 1407).
///
/// Agents are grouped by mapped name, case-insensitively. Different original
/// names in one group (`a.b`, `a-b`) block. A name used by one agent keeps
/// its plain ID. For a reused name, the live agent keeps the plain ID, or if
/// none is live the most recently retired one (ties: highest id); every
/// other agent gets a dated ID (see [`assign_dated`]).
fn map_owner_scope(
    owner_name: &str,
    agents: Vec<Agent>,
    issues: &mut Vec<String>,
) -> Vec<(Agent, String)> {
    let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, agent) in agents.iter().enumerate() {
        by_key
            .entry(map_name(&agent.name).to_lowercase())
            .or_default()
            .push(i);
    }
    // Every group's plain ID is taken by exactly one of its agents.
    let plain_ids: HashSet<String> = by_key.keys().cloned().collect();
    let mut segments: Vec<Option<String>> = vec![None; agents.len()];
    let mut dated = Vec::new();
    for (key, group) in &by_key {
        let plain = |i: usize| map_name(&agents[i].name);
        let describe_all = || {
            group
                .iter()
                .map(|&i| describe(EntityKind::Agent, agents[i].id, &agents[i].name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let originals: BTreeSet<String> = group
            .iter()
            .map(|&i| agents[i].name.to_lowercase())
            .collect();
        let live: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&i| agents[i].retired_at.is_none())
            .collect();
        let mut times = Vec::new();
        let mut unparseable = Vec::new();
        for &i in group {
            if let Some(text) = &agents[i].retired_at {
                match parse_retired_at(text) {
                    Some(t) => times.push((i, t)),
                    None => unparseable.push(format!(
                        "{} retired_at {text:?}",
                        describe(EntityKind::Agent, agents[i].id, &agents[i].name)
                    )),
                }
            }
        }
        let holder = if originals.len() > 1 {
            issues.push(format!(
                "different agent names map to `{owner_name}/{key}`: {}",
                describe_all()
            ));
            None
        } else if group.len() == 1 {
            Some(group[0])
        } else if live.len() > 1 {
            issues.push(format!(
                "more than one live agent maps to `{owner_name}/{key}`: {}",
                describe_all()
            ));
            None
        } else if !unparseable.is_empty() {
            issues.push(format!(
                "reused name `{owner_name}/{key}` needs dated IDs, but these retired_at \
                 values are not RFC3339 times: {}",
                unparseable.join(", ")
            ));
            None
        } else if let Some(&i) = live.first() {
            Some(i)
        } else {
            times
                .iter()
                .max_by_key(|&&(i, t)| (t, agents[i].id))
                .map(|&(i, _)| i)
        };
        let Some(holder) = holder else {
            // Blocked: report plain IDs only.
            for &i in group {
                segments[i] = Some(plain(i));
            }
            continue;
        };
        segments[holder] = Some(plain(holder));
        for (i, retired_at) in times {
            if i != holder {
                dated.push(Dated {
                    index: i,
                    base: plain(i),
                    retired_at,
                    level: DateLevel::Day,
                });
            }
        }
    }
    for (index, segment) in assign_dated(&plain_ids, dated, &agents) {
        segments[index] = Some(segment);
    }
    agents
        .into_iter()
        .zip(segments)
        .map(|(agent, segment)| (agent, segment.expect("every agent gets a segment")))
        .collect()
}

/// Dated IDs for retired agents of reused names, unique case-insensitively
/// among themselves and against every plain ID in the scope.
///
/// Every agent starts at `Day`. In rounds, every agent whose ID equals a plain
/// ID or another dated agent's ID moves to the next level, all at once, until
/// no ID clashes; so the result depends only on the inputs, not on order.
/// Agents that reach `Counter` then get `-2`, `-3`, ... in agents.id order,
/// each the lowest counter no ID already holds.
fn assign_dated(
    plain_ids: &HashSet<String>,
    mut dated: Vec<Dated>,
    agents: &[Agent],
) -> Vec<(usize, String)> {
    loop {
        let ids: Vec<String> = dated.iter().map(|d| d.segment().to_lowercase()).collect();
        let mut uses: HashMap<&str, usize> = HashMap::new();
        for id in &ids {
            *uses.entry(id.as_str()).or_default() += 1;
        }
        let clashing: Vec<usize> = (0..dated.len())
            .filter(|&n| dated[n].level != DateLevel::Counter)
            .filter(|&n| plain_ids.contains(&ids[n]) || uses[ids[n].as_str()] > 1)
            .collect();
        if clashing.is_empty() {
            break;
        }
        for n in clashing {
            dated[n].level = dated[n].level.next();
        }
    }
    let mut taken: HashSet<String> = plain_ids.clone();
    let mut out = Vec::new();
    for d in dated.iter().filter(|d| d.level != DateLevel::Counter) {
        taken.insert(d.segment().to_lowercase());
        out.push((d.index, d.segment()));
    }
    let mut counted: Vec<&Dated> = dated
        .iter()
        .filter(|d| d.level == DateLevel::Counter)
        .collect();
    counted.sort_by_key(|d| agents[d.index].id);
    for d in counted {
        let segment = (2..)
            .map(|n| format!("{}-{n}", d.segment()))
            .find(|s| !taken.contains(&s.to_lowercase()))
            .expect("an unbounded range finds a free counter");
        taken.insert(segment.to_lowercase());
        out.push((d.index, segment));
    }
    out
}

/// Legacy references without foreign keys that the cutover cannot resolve.
fn history_issues(conn: &Connection, issues: &mut Vec<String>) -> Result<()> {
    let checks = [
        (
            "messages whose sender does not exist",
            "SELECT 'message ' || m.id || ' from ' || m.from_kind || ' ' || m.from_id
             FROM messages m WHERE
               (m.from_kind = 'human' AND NOT EXISTS (SELECT 1 FROM humans WHERE id = m.from_id)) OR
               (m.from_kind = 'agent' AND NOT EXISTS (SELECT 1 FROM agents WHERE id = m.from_id))
             ORDER BY m.id",
        ),
        (
            "deliveries whose recipient does not exist",
            "SELECT 'message ' || d.message_id || ' to ' || d.recipient_kind || ' ' || d.recipient_id
             FROM deliveries d WHERE
               (d.recipient_kind = 'human' AND NOT EXISTS (SELECT 1 FROM humans WHERE id = d.recipient_id)) OR
               (d.recipient_kind = 'agent' AND NOT EXISTS (SELECT 1 FROM agents WHERE id = d.recipient_id))
             ORDER BY d.message_id",
        ),
        (
            "direct messages without exactly one delivery",
            "SELECT 'message ' || id || ' has ' || n || ' deliveries' FROM (
               SELECT m.id, (SELECT count(*) FROM deliveries d WHERE d.message_id = m.id) AS n
               FROM messages m WHERE m.to_group_id IS NULL)
             WHERE n != 1 ORDER BY id",
        ),
    ];
    for (what, sql) in checks {
        let found = strings(conn, sql)?;
        if !found.is_empty() {
            issues.push(format!("{} {what}: {}", found.len(), examples(&found)));
        }
    }
    Ok(())
}

/// Apply the SQL inside the caller's transaction and verify it before commit.
fn apply(tx: &Connection, preflight: &PreflightReport) -> Result<HubMigrationReport> {
    let messages = count(tx, "messages")?;
    let deliveries = count(tx, "deliveries")?;
    tx.execute_batch(
        "CREATE TEMP TABLE hub_migration_context (namespace TEXT NOT NULL);
         CREATE TEMP TABLE hub_native_map (
             kind TEXT NOT NULL CHECK(kind IN ('human', 'agent', 'group')),
             native_id INTEGER NOT NULL,
             stable_id TEXT NOT NULL UNIQUE COLLATE NOCASE,
             PRIMARY KEY(kind, native_id)
         );",
    )?;
    tx.execute(
        "INSERT INTO hub_migration_context VALUES (?1)",
        params![preflight.namespace],
    )?;
    let mut insert = tx.prepare("INSERT INTO hub_native_map VALUES (?1, ?2, ?3)")?;
    for m in &preflight.mappings {
        insert.execute(params![m.kind.as_str(), m.native_id, m.entity_id])?;
    }
    drop(insert);
    tx.execute_batch(HUB_MODEL_SQL)
        .context("applying the Hub model migration")?;
    let (new_messages, new_deliveries) = (count(tx, "messages")?, count(tx, "deliveries")?);
    if (new_messages, new_deliveries) != (messages, deliveries) {
        bail!(
            "Hub model migration copied {new_messages} of {messages} messages and \
             {new_deliveries} of {deliveries} deliveries"
        );
    }
    if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
        bail!("Hub model migration would leave invalid foreign-key references");
    }
    tx.execute_batch("DROP TABLE temp.hub_migration_context")?;
    tx.pragma_update(None, "user_version", HUB_MODEL_SCHEMA_VERSION as i64)?;
    Ok(HubMigrationReport {
        humans: preflight.count(EntityKind::Human),
        agents: preflight.count(EntityKind::Agent),
        groups: preflight.count(EntityKind::Group),
        messages,
        deliveries,
        renamed: preflight.renamed(),
    })
}

#[cfg(test)]
mod tests;
