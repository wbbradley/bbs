//! Immutable blocker versions, terminal events and operation receipts.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Kind, Principal};

pub struct Outcome {
    pub kind: String,
    pub actor: Principal,
    /// The acting agent's owner; none for a human actor.
    pub actor_owner: Option<String>,
    pub created_at: String,
    pub answer: Option<String>,
    /// The mail sent the creator: the answer, or a cancel notice; none when a
    /// cancel told no one.
    pub message_id: Option<i64>,
    /// Canonical JSON array of selected option IDs; none for a free-text answer.
    pub selected: Option<String>,
}

pub struct Row {
    pub id: i64,
    pub creator: Principal,
    pub creator_owner: Option<String>,
    pub body: String,
    pub created_at: String,
    pub root_id: i64,
    pub replaces: Option<i64>,
    pub replaced_by: Option<i64>,
    pub outcome: Option<Outcome>,
    /// Canonical JSON response options; none for a free-text blocker.
    pub choices: Option<String>,
}

/// The blocker query.
const SELECT: &str = "SELECT b.id,
    a.id, a.name, h.name, b.body, b.created_at, COALESCE(b.root_id, b.id),
    b.replaces, successor.id, e.kind, e.actor_kind, e.actor_id,
    CASE e.actor_kind WHEN 'human' THEN eh.name ELSE ea.name END,
    e.created_at, e.answer, e.message_id,
    actor_owner.name, b.choices, e.selected
    FROM blockers b JOIN agents a ON a.id = b.creator_id
    LEFT JOIN agent_owners owner ON owner.agent_id = a.id
    LEFT JOIN humans h ON h.id = owner.human_id
    LEFT JOIN blockers successor ON successor.replaces = b.id
    LEFT JOIN blocker_events e ON e.blocker_id = b.id AND e.kind != 'created'
    LEFT JOIN humans eh ON e.actor_kind = 'human' AND eh.id = e.actor_id
    LEFT JOIN agents ea ON e.actor_kind = 'agent' AND ea.id = e.actor_id
    LEFT JOIN agent_owners actor_link ON e.actor_kind = 'agent' AND actor_link.agent_id = e.actor_id
    LEFT JOIN humans actor_owner ON actor_owner.id = actor_link.human_id";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let outcome = if let Some(kind) = r.get::<_, Option<String>>(9)? {
        Some(Outcome {
            kind,
            actor: Principal {
                kind: if r.get::<_, String>(10)? == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                id: r.get(11)?,
                name: r.get(12)?,
            },
            actor_owner: r.get(16)?,
            created_at: r.get(13)?,
            answer: r.get(14)?,
            message_id: r.get(15)?,
            selected: r.get(18)?,
        })
    } else {
        None
    };
    Ok(Row {
        id: r.get(0)?,
        creator: Principal {
            kind: Kind::Agent,
            id: r.get(1)?,
            name: r.get(2)?,
        },
        creator_owner: r.get(3)?,
        body: r.get(4)?,
        created_at: r.get(5)?,
        root_id: r.get(6)?,
        replaces: r.get(7)?,
        replaced_by: r.get(8)?,
        outcome,
        choices: r.get(17)?,
    })
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Row>> {
    Ok(conn
        .query_row(&format!("{} WHERE b.id = ?1", SELECT), [id], read_row)
        .optional()?)
}

pub fn head(conn: &Connection, root: i64) -> Result<Row> {
    Ok(conn.query_row(
        &format!(
            "{} WHERE COALESCE(b.root_id, b.id) = ?1 AND successor.id IS NULL",
            SELECT
        ),
        [root],
        read_row,
    )?)
}

pub fn history(conn: &Connection, root: i64, before: i64, limit: usize) -> Result<Vec<Row>> {
    let mut query = conn.prepare(&format!(
        "{} WHERE COALESCE(b.root_id, b.id) = ?1 AND b.id < ?2 ORDER BY b.id DESC LIMIT ?3",
        SELECT
    ))?;
    Ok(query
        .query_map(params![root, before, limit as i64], read_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Counts and pages use exactly the same predicate: blockers an agent
/// created, or that a human's agents created.
const SCOPE: &str = "(?1 IS NULL OR b.creator_id = ?1)
    AND (?2 IS NULL OR EXISTS(SELECT 1 FROM agent_owners o
                              WHERE o.agent_id = b.creator_id AND o.human_id = ?2))";

pub enum ListMode {
    All { include_closed: bool, after: i64 },
    Closed { before: Option<i64> },
}

pub fn list(
    conn: &Connection,
    agent: Option<i64>,
    human: Option<i64>,
    mode: ListMode,
    limit: usize,
) -> Result<Vec<Row>> {
    let (predicate, order, cursor) = match mode {
        ListMode::All {
            include_closed,
            after,
        } => (
            if include_closed { "1" } else { "e.id IS NULL" },
            "ASC",
            Some(after),
        ),
        ListMode::Closed { before } => ("e.id IS NOT NULL", "DESC", before),
    };
    let comparison = if order == "DESC" { "<" } else { ">" };
    let mut query = conn.prepare(&format!(
        "{select} WHERE {scope} AND successor.id IS NULL
         AND ({predicate})
         AND (?3 IS NULL OR COALESCE(b.root_id, b.id) {comparison} ?3)
         ORDER BY COALESCE(b.root_id, b.id) {order} LIMIT ?4",
        select = SELECT,
        scope = SCOPE
    ))?;
    Ok(query
        .query_map(params![agent, human, cursor, limit as i64], read_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn active_count(conn: &Connection, agent: Option<i64>, human: Option<i64>) -> Result<u64> {
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM blockers b
             WHERE {SCOPE}
             AND NOT EXISTS (SELECT 1 FROM blockers s WHERE s.replaces = b.id)
             AND NOT EXISTS (SELECT 1 FROM blocker_events e
                             WHERE e.blocker_id = b.id AND e.kind != 'created')"
        ),
        params![agent, human],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

/// The newest blocker event's id, 0 with none: it grows on every blocker change.
pub fn revision(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT coalesce(max(id), 0) FROM blocker_events", [], |r| {
            r.get(0)
        })?,
    )
}

/// Each live agent's open blockers. One grouped query serves both the agent
/// list and live summaries.
pub fn agent_counts(conn: &Connection) -> Result<std::collections::HashMap<i64, u64>> {
    let mut query = conn.prepare_cached(
        "SELECT a.id, COUNT(*) FROM blockers b
         JOIN agents a ON a.id = b.creator_id AND a.retired_at IS NULL
         WHERE NOT EXISTS (SELECT 1 FROM blockers s WHERE s.replaces = b.id)
           AND NOT EXISTS (SELECT 1 FROM blocker_events e
                           WHERE e.blocker_id = b.id AND e.kind != 'created')
         GROUP BY a.id",
    )?;
    Ok(query
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub struct Version<'a> {
    pub body: &'a str,
    /// Canonical JSON from `blocker_choices::to_json`.
    pub choices: Option<&'a str>,
}

pub fn insert(
    conn: &Connection,
    creator: i64,
    version: Version<'_>,
    prior: Option<&Row>,
    now: &str,
    key: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO blockers (creator_id, body, replaces, root_id, created_at, request_key, choices)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            creator,
            version.body,
            prior.map(|p| p.id),
            prior.map(|p| p.root_id),
            now,
            key,
            version.choices
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub struct Answer<'a> {
    pub text: &'a str,
    pub selected: Option<&'a str>,
}

/// Record `kind` on blocker `id`. `message_id` is the mail it sent the
/// creator: an answer's (required) or a cancel's notice.
#[allow(clippy::too_many_arguments)]
pub fn event(
    conn: &Connection,
    id: i64,
    kind: &str,
    actor: &Principal,
    now: &str,
    answer: Option<Answer<'_>>,
    message_id: Option<i64>,
    key: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO blocker_events (blocker_id, kind, actor_kind, actor_id, created_at, answer, message_id, request_key, selected)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            kind,
            actor.kind.as_str(),
            actor.id,
            now,
            answer.as_ref().map(|a| a.text),
            message_id,
            key,
            answer.as_ref().and_then(|a| a.selected)
        ],
    )?;
    Ok(())
}

/// Requests are remembered on the immutable version or outcome they created.
/// Callers check both tables inside their IMMEDIATE transaction, so a key
/// cannot be reused for a different kind of write.
pub enum Operation {
    Put {
        id: i64,
        body: String,
        replaces: Option<i64>,
        choices: Option<String>,
    },
    Answer {
        id: i64,
        answer: String,
        message_id: i64,
        selected: Option<String>,
    },
    Cancel {
        id: i64,
        message_id: Option<i64>,
    },
}

impl Operation {
    pub fn id(&self) -> i64 {
        match self {
            Self::Put { id, .. } | Self::Answer { id, .. } | Self::Cancel { id, .. } => *id,
        }
    }
    pub fn message_id(&self) -> Option<i64> {
        match self {
            Self::Answer { message_id, .. } => Some(*message_id),
            Self::Cancel { message_id, .. } => *message_id,
            Self::Put { .. } => None,
        }
    }
}

pub fn operation(conn: &Connection, actor: &Principal, key: &str) -> Result<Option<Operation>> {
    if actor.kind == Kind::Agent {
        let created = conn.query_row(
            "SELECT id, body, replaces, choices FROM blockers WHERE creator_id = ?1 AND request_key = ?2",
            params![actor.id, key],
            |r| Ok(Operation::Put { id: r.get(0)?, body: r.get(1)?, replaces: r.get(2)?, choices: r.get(3)? }),
        ).optional()?;
        if created.is_some() {
            return Ok(created);
        }
    }
    Ok(conn.query_row(
        "SELECT blocker_id, kind, answer, message_id, selected FROM blocker_events WHERE actor_kind = ?1 AND actor_id = ?2 AND request_key = ?3",
        params![actor.kind.as_str(), actor.id, key],
        |r| {
            let id = r.get(0)?;
            if r.get::<_, String>(1)? == "answered" {
                Ok(Operation::Answer { id, answer: r.get(2)?, message_id: r.get(3)?, selected: r.get(4)? })
            } else { Ok(Operation::Cancel { id, message_id: r.get(3)? }) }
        },
    ).optional()?)
}
