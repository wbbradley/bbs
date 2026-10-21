//! Immutable blocker versions, terminal events and operation receipts.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Kind, Principal};

pub struct Outcome {
    pub kind: String,
    pub actor: Principal,
    pub created_at: String,
    pub answer: Option<String>,
    pub message_id: Option<i64>,
}

pub struct Row {
    pub id: i64,
    pub group_id: i64,
    pub group: String,
    pub archived: bool,
    pub creator: Principal,
    pub creator_owner: Option<String>,
    pub body: String,
    pub created_at: String,
    pub root_id: i64,
    pub replaces: Option<i64>,
    pub replaced_by: Option<i64>,
    pub outcome: Option<Outcome>,
}

const SELECT: &str = "SELECT b.id, b.group_id, g.name, g.archived_at,
    a.id, a.name, h.name, b.body, b.created_at, COALESCE(b.root_id, b.id),
    b.replaces, successor.id, e.kind, e.actor_kind, e.actor_id,
    CASE e.actor_kind WHEN 'human' THEN eh.name ELSE ea.name END,
    e.created_at, e.answer, e.message_id
    FROM blockers b JOIN bbs_groups g ON g.id = b.group_id
    JOIN agents a ON a.id = b.creator_id
    LEFT JOIN agent_owners owner ON owner.agent_id = a.id
    LEFT JOIN humans h ON h.id = owner.human_id
    LEFT JOIN blockers successor ON successor.replaces = b.id
    LEFT JOIN blocker_events e ON e.blocker_id = b.id AND e.kind != 'created'
    LEFT JOIN humans eh ON e.actor_kind = 'human' AND eh.id = e.actor_id
    LEFT JOIN agents ea ON e.actor_kind = 'agent' AND ea.id = e.actor_id";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let outcome = if let Some(kind) = r.get::<_, Option<String>>(12)? {
        Some(Outcome {
            kind,
            actor: Principal {
                kind: if r.get::<_, String>(13)? == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                id: r.get(14)?,
                name: r.get(15)?,
            },
            created_at: r.get(16)?,
            answer: r.get(17)?,
            message_id: r.get(18)?,
        })
    } else {
        None
    };
    Ok(Row {
        id: r.get(0)?,
        group_id: r.get(1)?,
        group: r.get(2)?,
        archived: r.get::<_, Option<String>>(3)?.is_some(),
        creator: Principal {
            kind: Kind::Agent,
            id: r.get(4)?,
            name: r.get(5)?,
        },
        creator_owner: r.get(6)?,
        body: r.get(7)?,
        created_at: r.get(8)?,
        root_id: r.get(9)?,
        replaces: r.get(10)?,
        replaced_by: r.get(11)?,
        outcome,
    })
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Row>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE b.id = ?1"), [id], read_row)
        .optional()?)
}

pub fn head(conn: &Connection, root: i64) -> Result<Row> {
    Ok(conn.query_row(
        &format!("{SELECT} WHERE COALESCE(b.root_id, b.id) = ?1 AND successor.id IS NULL"),
        [root],
        read_row,
    )?)
}

pub fn history(conn: &Connection, root: i64, before: i64, limit: usize) -> Result<Vec<Row>> {
    let mut query = conn.prepare(&format!(
        "{SELECT} WHERE COALESCE(b.root_id, b.id) = ?1 AND b.id < ?2 ORDER BY b.id DESC LIMIT ?3"
    ))?;
    Ok(query
        .query_map(params![root, before, limit as i64], read_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Counts and pages use exactly the same current membership/group predicate.
const SCOPE: &str = "(?1 IS NULL OR g.name = ?1)
    AND (?2 IS NULL OR b.group_id IN (
      SELECT m.group_id FROM group_members m JOIN bbs_groups scope_group ON scope_group.id = m.group_id
      WHERE m.agent_id = ?2 AND scope_group.archived_at IS NULL))";

pub fn list(
    conn: &Connection,
    group: Option<&str>,
    agent: Option<i64>,
    closed: bool,
    after: i64,
    limit: usize,
) -> Result<Vec<Row>> {
    let mut query = conn.prepare(&format!(
        "{SELECT} WHERE {SCOPE} AND successor.id IS NULL
         AND (?3 OR (e.id IS NULL AND g.archived_at IS NULL))
         AND COALESCE(b.root_id, b.id) > ?4
         ORDER BY COALESCE(b.root_id, b.id) LIMIT ?5"
    ))?;
    Ok(query
        .query_map(params![group, agent, closed, after, limit as i64], read_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn active_count(conn: &Connection, group: Option<&str>, agent: Option<i64>) -> Result<u64> {
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM blockers b JOIN bbs_groups g ON g.id = b.group_id
             WHERE {SCOPE} AND g.archived_at IS NULL
             AND NOT EXISTS (SELECT 1 FROM blockers s WHERE s.replaces = b.id)
             AND NOT EXISTS (SELECT 1 FROM blocker_events e
                             WHERE e.blocker_id = b.id AND e.kind != 'created')"
        ),
        params![group, agent],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

pub fn human_owns_member(conn: &Connection, group: i64, human: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM group_members m
         JOIN agents a ON a.id = m.agent_id
         JOIN agent_owners o ON o.agent_id = a.id
         JOIN bbs_groups g ON g.id = m.group_id
         WHERE m.group_id = ?1 AND o.human_id = ?2
         AND a.retired_at IS NULL AND g.archived_at IS NULL)",
        params![group, human],
        |r| r.get(0),
    )?)
}

pub fn insert(
    conn: &Connection,
    group: i64,
    creator: i64,
    body: &str,
    prior: Option<&Row>,
    now: &str,
    key: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO blockers (group_id, creator_id, body, replaces, root_id, created_at, request_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            group,
            creator,
            body,
            prior.map(|p| p.id),
            prior.map(|p| p.root_id),
            now,
            key
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn event(
    conn: &Connection,
    id: i64,
    kind: &str,
    actor: &Principal,
    now: &str,
    answer: Option<(&str, i64)>,
    key: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO blocker_events (blocker_id, kind, actor_kind, actor_id, created_at, answer, message_id, request_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, kind, actor.kind.as_str(), actor.id, now, answer.map(|a| a.0), answer.map(|a| a.1), key],
    )?;
    Ok(())
}

/// Requests are remembered on the immutable version or outcome they created.
/// Callers check both tables inside their IMMEDIATE transaction, so a key
/// cannot be reused for a different kind of write.
pub enum Operation {
    Put {
        id: i64,
        group: i64,
        body: String,
        replaces: Option<i64>,
    },
    Answer {
        id: i64,
        answer: String,
        message_id: i64,
    },
    Cancel {
        id: i64,
    },
}

impl Operation {
    pub fn id(&self) -> i64 {
        match self {
            Self::Put { id, .. } | Self::Answer { id, .. } | Self::Cancel { id } => *id,
        }
    }
    pub fn message_id(&self) -> Option<i64> {
        match self {
            Self::Answer { message_id, .. } => Some(*message_id),
            _ => None,
        }
    }
}

pub fn operation(conn: &Connection, actor: &Principal, key: &str) -> Result<Option<Operation>> {
    if actor.kind == Kind::Agent {
        let created = conn.query_row(
            "SELECT id, group_id, body, replaces FROM blockers WHERE creator_id = ?1 AND request_key = ?2",
            params![actor.id, key],
            |r| Ok(Operation::Put { id: r.get(0)?, group: r.get(1)?, body: r.get(2)?, replaces: r.get(3)? }),
        ).optional()?;
        if created.is_some() {
            return Ok(created);
        }
    }
    Ok(conn.query_row(
        "SELECT blocker_id, kind, answer, message_id FROM blocker_events WHERE actor_kind = ?1 AND actor_id = ?2 AND request_key = ?3",
        params![actor.kind.as_str(), actor.id, key],
        |r| {
            let id = r.get(0)?;
            if r.get::<_, String>(1)? == "answered" {
                Ok(Operation::Answer { id, answer: r.get(2)?, message_id: r.get(3)? })
            } else { Ok(Operation::Cancel { id }) }
        },
    ).optional()?)
}
