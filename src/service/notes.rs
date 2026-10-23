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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service;

    struct Board {
        conn: Connection,
        will: Caller,
        ada: Caller,
        mateo: (Caller, i64),
        zed: (Caller, i64),
    }

    fn human(conn: &mut Connection, name: &str) -> Caller {
        service::ensure_human(conn, proto::EnsureHumanRequest { name: name.into() }).unwrap();
        Caller {
            human: Some(name.into()),
            ..Default::default()
        }
    }

    fn agent(conn: &mut Connection, owner: &Caller, name: &str) -> (Caller, i64) {
        let token = service::register(
            conn,
            owner,
            proto::RegisterRequest {
                name: Some(name.into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let caller = Caller {
            token: Some(token),
            ..Default::default()
        };
        let id = service::whoami(conn, &caller).unwrap().id;
        (caller, id)
    }

    impl Board {
        fn new() -> Board {
            let mut conn = Connection::open_in_memory().unwrap();
            db::migrate(&mut conn).unwrap();
            let will = human(&mut conn, "will");
            let ada = human(&mut conn, "ada");
            let mateo = agent(&mut conn, &will, "Mateo");
            let zed = agent(&mut conn, &ada, "Zed");
            Board {
                conn,
                will,
                ada,
                mateo,
                zed,
            }
        }

        fn put(&mut self, who: &Caller, agent_id: i64, body: &str) -> Result<proto::AgentNote> {
            put_agent_note(
                &mut self.conn,
                who,
                proto::PutAgentNoteRequest {
                    agent_id,
                    body: body.into(),
                },
            )
            .map(|r| r.note.unwrap())
        }

        fn list(&mut self, who: &Caller, after: u64, limit: u32) -> proto::ListAgentNotesResponse {
            list_agent_notes(
                &mut self.conn,
                who,
                proto::ListAgentNotesRequest {
                    after_revision: after,
                    limit,
                },
            )
            .unwrap()
        }

        fn revision(&mut self, who: &Caller) -> Option<u64> {
            service::agent_summary(&mut self.conn, who, proto::AgentSummaryRequest {})
                .unwrap()
                .notes_revision
        }
    }

    #[test]
    fn notes_are_private_to_each_human_and_refused_to_agents() {
        let mut b = Board::new();
        let (mateo, zed) = (b.mateo.1, b.zed.1);
        // Any owner's live agent: will notes ada's Zed and his own Mateo.
        let will = b.will.clone();
        let ada = b.ada.clone();
        b.put(&will, zed, "will on zed").unwrap();
        b.put(&will, mateo, "will on mateo").unwrap();
        b.put(&ada, zed, "ada on zed").unwrap();
        let mine: Vec<_> = b
            .list(&will, 0, 0)
            .notes
            .into_iter()
            .map(|n| n.body)
            .collect();
        assert_eq!(mine, ["will on zed", "will on mateo"]);
        let hers: Vec<_> = b
            .list(&ada, 0, 0)
            .notes
            .into_iter()
            .map(|n| n.body)
            .collect();
        assert_eq!(hers, ["ada on zed"]);
        // Revisions are per human.
        assert_eq!(b.revision(&will), Some(2));
        assert_eq!(b.revision(&ada), Some(1));
        // Agents can neither write, list nor see a revision.
        let agent = b.mateo.0.clone();
        assert!(b.put(&agent, zed, "agent").is_err());
        assert!(
            list_agent_notes(&mut b.conn, &agent, proto::ListAgentNotesRequest::default()).is_err()
        );
        // No caller at all is refused.
        assert!(b.put(&Caller::default(), zed, "nobody").is_err());
    }

    #[test]
    fn puts_validate_size_identity_and_skip_unchanged_bodies() {
        let mut b = Board::new();
        let will = b.will.clone();
        let mateo = b.mateo.1;
        assert!(b.put(&will, 0, "x").is_err());
        assert!(b.put(&will, 999, "x").is_err());
        // 32768 bytes exactly, in multibyte characters, is allowed; one more is not.
        let exact = "é".repeat(MAX_NOTE_BYTES / 2);
        assert_eq!(exact.len(), MAX_NOTE_BYTES);
        assert_eq!(b.put(&will, mateo, &exact).unwrap().body, exact);
        assert!(b.put(&will, mateo, &format!("{exact}x")).is_err());
        let before = b.revision(&will);
        // The same body again is a no-op with no revision churn.
        let same = b.put(&will, mateo, &exact).unwrap();
        assert_eq!(Some(same.revision), before);
        assert_eq!(b.revision(&will), before);
        // An empty body clears the note and is listed as a tombstone.
        let cleared = b.put(&will, mateo, "").unwrap();
        assert_eq!(cleared.revision, before.unwrap() + 1);
        let listed = b.list(&will, before.unwrap(), 0).notes;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].body.is_empty() && !listed[0].retired);
    }

    #[test]
    fn retirement_keeps_the_body_signals_every_human_and_never_moves_to_a_new_name() {
        let mut b = Board::new();
        let will = b.will.clone();
        let ada = b.ada.clone();
        let (old_mateo, old_id) = b.mateo.clone();
        b.put(&will, old_id, "will on old mateo").unwrap();
        b.put(&ada, old_id, "ada on old mateo").unwrap();
        let (will_before, ada_before) = (b.revision(&will).unwrap(), b.revision(&ada).unwrap());
        service::bye(&mut b.conn, &old_mateo).unwrap();
        // Every affected human's signal moves, even with no live agent rows.
        assert_eq!(b.revision(&will), Some(will_before + 1));
        assert_eq!(b.revision(&ada), Some(ada_before + 1));
        let row = &b.list(&will, will_before, 0).notes[0];
        assert!(row.retired);
        assert_eq!(row.body, "will on old mateo");
        assert_eq!(row.agent_id, old_id);
        // Retired: no further writes, even of the same body.
        assert!(b.put(&will, old_id, "will on old mateo").is_err());
        assert!(b.put(&will, old_id, "new text").is_err());
        // A new Mateo is a new id with no note.
        let (_, new_id) = agent(&mut b.conn, &will, "Mateo");
        assert_ne!(new_id, old_id);
        assert!(
            b.list(&will, 0, 0)
                .notes
                .iter()
                .all(|n| n.agent_id != new_id)
        );
        b.put(&will, new_id, "will on new mateo").unwrap();
        let all = b.list(&will, 0, 0).notes;
        let old = all.iter().find(|n| n.agent_id == old_id).unwrap();
        assert_eq!(
            (old.body.as_str(), old.retired),
            ("will on old mateo", true)
        );
        let new = all.iter().find(|n| n.agent_id == new_id).unwrap();
        assert_eq!(
            (new.body.as_str(), new.retired),
            ("will on new mateo", false)
        );
        // A human with no notes sees Some(0), never absent.
        let lu = human(&mut b.conn, "lu");
        assert_eq!(b.revision(&lu), Some(0));
        assert!(b.list(&lu, 0, 0).notes.is_empty());
    }

    #[test]
    fn pages_follow_revisions_and_never_skip_a_change_made_while_paging() {
        let mut b = Board::new();
        let will = b.will.clone();
        let owner = will.clone();
        let ids: Vec<i64> = (0..5)
            .map(|i| agent(&mut b.conn, &owner, &format!("A{i}")).1)
            .collect();
        for id in &ids {
            b.put(&will, *id, &format!("note {id}")).unwrap();
        }
        let first = b.list(&will, 0, 2);
        assert_eq!(first.notes.len(), 2);
        assert!(first.has_more);
        assert_eq!(first.next_after_revision, first.notes[1].revision);
        // An earlier note changes between pages: it moves past the cursor.
        b.put(&will, ids[0], "edited").unwrap();
        let mut seen: Vec<_> = first.notes.iter().map(|n| n.agent_id).collect();
        let mut after = first.next_after_revision;
        loop {
            let page = b.list(&will, after, 2);
            seen.extend(page.notes.iter().map(|n| n.agent_id));
            after = page.next_after_revision;
            if !page.has_more {
                break;
            }
        }
        // All five, with the edited one delivered again at its new revision.
        assert_eq!(seen.iter().filter(|id| **id == ids[0]).count(), 2);
        for id in &ids {
            assert!(seen.contains(id));
        }
        // An empty page keeps the cursor where it was.
        assert_eq!(b.list(&will, after, 2).next_after_revision, after);
        assert!(
            list_agent_notes(
                &mut b.conn,
                &will,
                proto::ListAgentNotesRequest {
                    after_revision: 0,
                    limit: 65
                }
            )
            .is_err()
        );
    }
}
