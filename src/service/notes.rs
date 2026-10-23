//! Private per-human agent notes (agent-notes.v1). The human is always the
//! authenticated caller; no request field names one.

use anyhow::{Result, bail};
use rusqlite::Connection;

use super::{Caller, with_ctx};
use crate::db::{self, notes as store};
use crate::proto;

pub const MAX_NOTE_BYTES: usize = 32 * 1024;
const MAX_PAGE: u32 = 64;

fn note(row: store::Row) -> proto::AgentNote {
    proto::AgentNote {
        agent_id: row.agent_id,
        agent: row.agent,
        retired: row.retired,
        body: row.body,
        revision: row.revision as u64,
        updated_at: row.updated_at,
    }
}

/// The caller's notes changed after `after_revision`, retired ones included
/// so a client can hide them; empty bodies are cleared notes.
pub fn list_agent_notes(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ListAgentNotesRequest,
) -> Result<proto::ListAgentNotesResponse> {
    let limit = match req.limit {
        0 => MAX_PAGE,
        n if n > MAX_PAGE => bail!("agent note limit must be 1 to {MAX_PAGE}"),
        n => n,
    } as usize;
    let Ok(after) = i64::try_from(req.after_revision) else {
        bail!("agent note cursor is out of range");
    };
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let mut rows = store::list(ctx.conn, me.id, after, limit + 1)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_after_revision = rows
            .last()
            .map_or(req.after_revision, |r| r.revision as u64);
        Ok(proto::ListAgentNotesResponse {
            notes: rows.into_iter().map(note).collect(),
            has_more,
            next_after_revision,
        })
    })
}

/// Replace the caller's note on one live agent, last write wins. The same
/// body again changes nothing and returns the stored note.
pub fn put_agent_note(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::PutAgentNoteRequest,
) -> Result<proto::PutAgentNoteResponse> {
    if req.body.len() > MAX_NOTE_BYTES {
        bail!("an agent note is at most {MAX_NOTE_BYTES} UTF-8 bytes");
    }
    if req.agent_id <= 0 {
        bail!("agent note needs a positive agent id");
    }
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        // A retired or unknown agent is refused before any no-op, so a
        // replacement reusing its name can never receive this note.
        if !db::agent_is_live(ctx.conn, req.agent_id)? {
            bail!("agent notes need a live agent; this one is unknown or retired");
        }
        let current = store::get(ctx.conn, me.id, req.agent_id)?;
        if current.as_ref().is_none_or(|c| c.body != req.body) {
            store::put(ctx.conn, me.id, req.agent_id, &req.body, &ctx.now_s)?;
        }
        let row = store::get(ctx.conn, me.id, req.agent_id)?.expect("just written or present");
        Ok(proto::PutAgentNoteResponse {
            note: Some(note(row)),
        })
    })
}
