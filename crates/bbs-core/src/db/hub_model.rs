//! The Hub model cutover: schema 26 to 27, run only by an operator.
//!
//! Startup never applies it (it is not in `MIGRATIONS`), and a binary that only
//! knows schema 26 refuses a migrated database. The cutover moves messages and
//! deliveries onto Hub's entity/address model and gives every human, agent and
//! group one entity ID, computed here from its BBS name (BBS blocker 1396):
//!
//! - `.` becomes `-`; any other character outside `[A-Za-z0-9_-]` blocks.
//! - Humans, groups and ownerless agents get a top-level ID. They share one
//!   space, compared case-insensitively; any clash blocks.
//! - An owned agent gets `owner/name`. Within one owner, a name reused over
//!   time keeps its base ID for the oldest agent and gets `-2`, `-3`, ... for
//!   later ones; different names that map to one ID (`a.b`, `a-b`) block.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};

use super::schema_version;

/// The only schema the cutover starts from.
pub const HUB_MODEL_SOURCE_VERSION: usize = 26;
/// The schema the cutover produces.
pub const HUB_MODEL_SCHEMA_VERSION: usize = 27;

const HUB_MODEL_SQL: &str = include_str!("../../migrations/0027_hub_model.sql");
const MAX_ENTITY_ID: usize = 128;
/// How many offending records one preflight issue lists.
const MAX_EXAMPLES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HubKind {
    Human,
    Agent,
    Group,
}

impl HubKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HubKind::Human => "human",
            HubKind::Agent => "agent",
            HubKind::Group => "group",
        }
    }
}

/// One native record and the entity ID the cutover gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityMapping {
    pub kind: HubKind,
    pub native_id: i64,
    /// The BBS name, kept as the entity's display name.
    pub name: String,
    pub entity_id: String,
    /// A retired agent or an archived group.
    pub retired: bool,
}

impl EntityMapping {
    /// Whether the ID's name part differs from the BBS name: a `.` became `-`
    /// or a reused name got a numeric suffix.
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

    pub fn count(&self, kind: HubKind) -> usize {
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
    /// Mappings that needed `.` to `-` or a numeric suffix.
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

/// Run the cutover from schema 26 to 27 in one transaction. Refuses, leaving
/// the database unchanged at schema 26, on any preflight issue or failed check.
pub fn migrate_to_hub_model(conn: &mut Connection, namespace: &str) -> Result<HubMigrationReport> {
    validate_namespace(namespace)?;
    require_source_version(conn)?;
    if !conn.is_autocommit() {
        bail!("the Hub model migration requires a connection without an active transaction");
    }
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

fn describe(kind: HubKind, id: i64, name: &str) -> String {
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
        top_level.push((HubKind::Human, id, name, false));
    }
    let mut stmt =
        conn.prepare("SELECT id, name, archived_at IS NOT NULL FROM bbs_groups ORDER BY id")?;
    for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
        let (id, name, archived) = row?;
        top_level.push((HubKind::Group, id, name, archived));
    }
    let mut owned: BTreeMap<i64, Vec<(i64, String, bool)>> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.retired_at IS NOT NULL, o.human_id
         FROM agents a LEFT JOIN agent_owners o ON o.agent_id = a.id ORDER BY a.id",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?))
    })? {
        let (id, name, retired, owner) = row?;
        match owner {
            Some(owner) => owned.entry(owner).or_default().push((id, name, retired)),
            None => top_level.push((HubKind::Agent, id, name, retired)),
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

    // Owned agents: per owner scope, in creation (id) order.
    for (owner, agents) in owned {
        let Some(owner_name) = owner_names.get(&owner) else {
            // Also a foreign-key issue; no scope to place these agents in.
            issues.push(format!("agents owned by missing human {owner}"));
            continue;
        };
        let mut by_key: BTreeMap<String, Vec<(i64, String, bool)>> = BTreeMap::new();
        for agent in agents {
            by_key
                .entry(map_name(&agent.1).to_lowercase())
                .or_default()
                .push(agent);
        }
        let base_keys: HashSet<String> = by_key.keys().cloned().collect();
        let mut assigned = HashSet::new();
        for (key, agents) in by_key {
            let originals: BTreeSet<String> = agents.iter().map(|a| a.1.to_lowercase()).collect();
            if originals.len() > 1 {
                let records: Vec<_> = agents
                    .iter()
                    .map(|(id, name, _)| describe(HubKind::Agent, *id, name))
                    .collect();
                issues.push(format!(
                    "different agent names map to `{owner_name}/{key}`: {}",
                    records.join(", ")
                ));
            }
            for (i, (id, name, retired)) in agents.into_iter().enumerate() {
                let base = map_name(&name);
                let mut segment = base.clone();
                if i > 0 {
                    // A reused name: the next suffix no other agent here holds.
                    segment = (2..)
                        .map(|n| format!("{base}-{n}"))
                        .find(|s| {
                            let k = s.to_lowercase();
                            !base_keys.contains(&k) && !assigned.contains(&k)
                        })
                        .expect("an unbounded range finds a free suffix");
                }
                assigned.insert(segment.to_lowercase());
                let m = EntityMapping {
                    kind: HubKind::Agent,
                    native_id: id,
                    name,
                    entity_id: format!("{owner_name}/{segment}"),
                    retired,
                };
                check_id(&m, &segment, issues);
                mappings.push(m);
            }
        }
    }
    mappings.sort_by_key(|m| (m.kind, m.native_id));
    Ok(mappings)
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
        humans: preflight.count(HubKind::Human),
        agents: preflight.count(HubKind::Agent),
        groups: preflight.count(HubKind::Group),
        messages,
        deliveries,
        renamed: preflight.renamed(),
    })
}

#[cfg(test)]
mod tests;
