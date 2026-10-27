//! Private per-human agent notes. Rows are never deleted.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub struct Row {
    pub agent_id: i64,
    pub agent: String,
    pub retired: bool,
    pub body: String,
    pub revision: i64,
    pub updated_at: String,
}

const SELECT: &str = "SELECT n.agent_id, a.name, a.retired_at IS NOT NULL, n.body, n.revision,
    n.updated_at FROM agent_notes n JOIN agents a ON a.id = n.agent_id";

fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        agent_id: r.get(0)?,
        agent: r.get(1)?,
        retired: r.get(2)?,
        body: r.get(3)?,
        revision: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

/// The human's newest note revision, 0 with none.
pub fn max_revision(conn: &Connection, human: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(revision), 0) FROM agent_notes WHERE human_id = ?1",
        [human],
        |r| r.get(0),
    )?)
}

pub fn get(conn: &Connection, human: i64, agent: i64) -> Result<Option<Row>> {
    Ok(conn
        .query_row(
            &format!("{SELECT} WHERE n.human_id = ?1 AND n.agent_id = ?2"),
            params![human, agent],
            read,
        )
        .optional()?)
}

/// Notes changed after `after`, ascending by revision.
pub fn list(conn: &Connection, human: i64, after: i64, limit: usize) -> Result<Vec<Row>> {
    let mut query = conn.prepare(&format!(
        "{SELECT} WHERE n.human_id = ?1 AND n.revision > ?2 ORDER BY n.revision LIMIT ?3"
    ))?;
    Ok(query
        .query_map(params![human, after, limit as i64], read)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Replace the human's note on `agent` with the next revision.
pub fn put(conn: &Connection, human: i64, agent: i64, body: &str, now: &str) -> Result<()> {
    let revision = max_revision(conn, human)? + 1;
    conn.execute(
        "INSERT INTO agent_notes (human_id, agent_id, body, revision, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (human_id, agent_id) DO UPDATE SET
           body = excluded.body, revision = excluded.revision, updated_at = excluded.updated_at",
        params![human, agent, body, revision, now],
    )?;
    Ok(())
}

/// On retirement, give each human's note on `agent` that human's next
/// revision, keeping its body, so every affected human's sync sees it retire.
pub fn retire(conn: &Connection, agent: i64, now: &str) -> Result<()> {
    let humans: Vec<i64> = conn
        .prepare("SELECT human_id FROM agent_notes WHERE agent_id = ?1 ORDER BY human_id")?
        .query_map([agent], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for human in humans {
        let revision = max_revision(conn, human)? + 1;
        conn.execute(
            "UPDATE agent_notes SET revision = ?3, updated_at = ?4
             WHERE human_id = ?1 AND agent_id = ?2",
            params![human, agent, revision, now],
        )?;
    }
    Ok(())
}
