//! Durable per-owner restart barriers. Reading mail is not completing a turn:
//! the runner must separately acknowledge the final notice after becoming idle.
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Caller, Ctx, fmt_time, with_ctx};
use crate::{db, proto};

const NOTICE: &str = "BBS restart preparation: delivery of new messages is paused. Finish your active turns and queued work, then become idle. Your headless process may be restarted. Record any long-running background tasks that you will need to restart or check after restart. Delivery resumes only when your human explicitly resumes it.";

pub fn agent_drain(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AgentDrainRequest,
) -> Result<proto::AgentDrainResponse> {
    agent_drain_with_changes(conn, caller, req).map(|(response, _)| response)
}

/// The transport wakes watchers only for changes to drain state or notifications,
/// never for an unchanged status poll or an identity's debounced activity touch.
pub fn agent_drain_with_changes(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::AgentDrainRequest,
) -> Result<(proto::AgentDrainResponse, bool)> {
    with_ctx(conn, caller, |ctx| {
        let me = ctx.human()?;
        let mut changed = false;
        match req.action.as_str() {
            "" => {}
            "resume" => {
                ctx.conn.execute(
                    "DELETE FROM agent_drain_targets WHERE human_id = ?1",
                    [me.id],
                )?;
                changed = ctx
                    .conn
                    .execute("DELETE FROM agent_drains WHERE human_id = ?1", [me.id])?
                    > 0;
            }
            "start" => {
                let exists: bool = ctx.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_drains WHERE human_id = ?1)",
                    [me.id],
                    |r| r.get(0),
                )?;
                if !exists {
                    changed = true;
                    // Snapshot every live owned agent, including offline/older runners.
                    // They must never silently disappear from the safety assessment.
                    let ids = ctx.conn.prepare("SELECT a.id FROM agents a JOIN agent_owners o ON o.agent_id = a.id WHERE o.human_id = ?1 AND a.retired_at IS NULL ORDER BY a.id")?
                        .query_map([me.id], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
                    let deadline = fmt_time(ctx.now + chrono::Duration::minutes(20));
                    ctx.conn.execute("INSERT INTO agent_drains(human_id, watermark, deadline, status) VALUES (?1, 0, ?2, 'draining')", params![me.id, deadline])?;
                    for id in ids {
                        let message = db::insert_message(ctx.conn, &me, None, NOTICE, &ctx.now_s)?;
                        db::insert_delivery(ctx.conn, message, db::Kind::Agent, id)?;
                        ctx.conn.execute("INSERT INTO agent_drain_targets(human_id, agent_id, final_message_id) VALUES (?1, ?2, ?3)", params![me.id, id, message])?;
                    }
                    ctx.conn.execute("UPDATE agent_drains SET watermark = (SELECT coalesce(max(id), 0) FROM messages) WHERE human_id = ?1", [me.id])?;
                }
            }
            _ => bail!("agent drain action must be start, resume, or empty"),
        }
        changed |= reconcile(ctx)? > 0;
        Ok((snapshot(ctx, me.id)?, changed))
    })
}

pub fn report_drain(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReportDrainRequest,
) -> Result<proto::Empty> {
    report_drain_with_changes(conn, caller, req).map(|(response, _)| response)
}

pub fn report_drain_with_changes(
    conn: &mut Connection,
    caller: &Caller,
    req: proto::ReportDrainRequest,
) -> Result<(proto::Empty, bool)> {
    if req.completed_through < 0 || req.side_completed_through < 0 {
        bail!("completed message and side request ids must not be negative");
    }
    with_ctx(conn, caller, |ctx| {
        let me = ctx.agent()?;
        // A side question's cancelled/ended status records owner intent, not
        // provider quiescence. Only this runner-confirmed cursor clears it.
        let updated = ctx.conn.execute(
            "WITH observed AS (
                SELECT coalesce(max(id), 0) AS through_id FROM side_requests WHERE agent_id = ?1
             )
             UPDATE agent_drain_targets SET completed = 1,
                side_completed_through = max(side_completed_through, min(?3, (SELECT through_id FROM observed)))
             WHERE agent_id = ?1 AND final_message_id <= ?2
                AND (completed = 0 OR side_completed_through < min(?3, (SELECT through_id FROM observed)))",
            params![me.id, req.completed_through, req.side_completed_through],
        )?;
        let reconciled = reconcile(ctx)?;
        Ok((proto::Empty {}, updated > 0 || reconciled > 0))
    })
}

fn snapshot(ctx: &Ctx<'_>, human: i64) -> Result<proto::AgentDrainResponse> {
    let row = ctx
        .conn
        .query_row(
            "SELECT watermark, deadline, status FROM agent_drains WHERE human_id = ?1",
            [human],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((watermark, deadline, status)) = row else {
        return Ok(proto::AgentDrainResponse {
            status: "running".into(),
            ..Default::default()
        });
    };
    let agents = ctx.conn.prepare("SELECT t.agent_id, a.name, t.completed,
        (SELECT count(*) FROM deliveries d WHERE d.recipient_kind = 'agent' AND d.recipient_id = t.agent_id AND d.message_id <= ?2 AND d.archived_at IS NULL),
        EXISTS(SELECT 1 FROM side_requests s WHERE s.agent_id = t.agent_id AND (s.status = 'open' OR s.id > t.side_completed_through))
        FROM agent_drain_targets t JOIN agents a ON a.id = t.agent_id WHERE t.human_id = ?1 ORDER BY lower(a.name)")?
        .query_map(params![human, watermark], |r| Ok(proto::AgentDrainTarget { agent_id: r.get(0)?, name: r.get(1)?, completed: r.get(2)?, unread: r.get(3)?, side_pending: r.get(4)? }))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(proto::AgentDrainResponse {
        frozen: true,
        status,
        watermark,
        deadline,
        agents,
    })
}

// Called even when no browser is open. Completion and its notification commit
// together, so retries and server restarts cannot duplicate the notification.
pub fn check_agent_drains(conn: &mut Connection, now: DateTime<Utc>) -> Result<usize> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ctx = Ctx {
        conn: &tx,
        now,
        now_s: fmt_time(now),
        caller: &super::NO_CALLER,
    };
    let changed = reconcile(&ctx)?;
    tx.commit()?;
    Ok(changed)
}

fn reconcile(ctx: &Ctx<'_>) -> Result<usize> {
    let rows = ctx.conn.prepare("SELECT r.human_id, h.name FROM agent_drains r JOIN humans h ON h.id = r.human_id WHERE r.status = 'draining'")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
    let mut changed = 0;
    for (id, name) in rows {
        let state = snapshot(ctx, id)?;
        let ready = state
            .agents
            .iter()
            .all(|a| a.completed && a.unread == 0 && !a.side_pending);
        if !ready && state.deadline > ctx.now_s {
            continue;
        }
        let status = if ready { "ready" } else { "timed_out" };
        let pending: Vec<_> = state
            .agents
            .iter()
            .filter(|a| !a.completed || a.unread > 0 || a.side_pending)
            .map(|a| a.name.as_str())
            .collect();
        let body = if ready {
            "BBS restart preparation complete: all agents read their mail through the watermark and reported their turns and reply queues finished; side questions have also finished. It is safe to restart your headless agents. After restarting, resume delivery in Settings.".to_string()
        } else {
            format!(
                "BBS restart preparation timed out after 20 minutes. Restart is as safe as we can confirm, but these agents have unread mail, active side questions, or have not confirmed completed turns: {}. Check them before restarting if possible. Delivery remains paused until you resume it in Settings.",
                pending.join(", ")
            )
        };
        let human = db::find_human(ctx.conn, &name)?.expect("human joined above");
        let message = db::insert_message(ctx.conn, &human, None, &body, &ctx.now_s)?;
        db::insert_delivery(ctx.conn, message, db::Kind::Human, id)?;
        ctx.conn.execute(
            "UPDATE agent_drains SET status = ?2 WHERE human_id = ?1",
            params![id, status],
        )?;
        changed += 1;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service;

    struct Board {
        conn: Connection,
        owner: Caller,
        other: Caller,
        agent: Caller,
        id: i64,
    }
    impl Board {
        fn new() -> Self {
            let mut conn = Connection::open_in_memory().unwrap();
            db::migrate(&mut conn).unwrap();
            let mut human = |name: &str| {
                service::ensure_human(&mut conn, proto::EnsureHumanRequest { name: name.into() })
                    .unwrap();
                Caller {
                    human: Some(name.into()),
                    ..Default::default()
                }
            };
            let owner = human("will");
            let other = human("ada");
            let token = service::register(
                &mut conn,
                &owner,
                proto::RegisterRequest {
                    name: Some("worker".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token;
            let agent = Caller {
                token: Some(token),
                ..Default::default()
            };
            let id = service::whoami(&mut conn, &agent).unwrap().id;
            Self {
                conn,
                owner,
                other,
                agent,
                id,
            }
        }
        fn drain(&mut self, action: &str) -> proto::AgentDrainResponse {
            agent_drain(
                &mut self.conn,
                &self.owner,
                proto::AgentDrainRequest {
                    action: action.into(),
                },
            )
            .unwrap()
        }
        fn send(&mut self, body: &str) -> i64 {
            service::send(
                &mut self.conn,
                &self.owner,
                proto::SendRequest {
                    to: "worker".into(),
                    body: body.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .id
            .unwrap()
        }
        fn read(&mut self) -> Vec<proto::Message> {
            service::read(&mut self.conn, &self.agent, proto::ReadRequest::default())
                .unwrap()
                .messages
        }
        fn complete(&mut self, id: i64) {
            report_drain(
                &mut self.conn,
                &self.agent,
                proto::ReportDrainRequest {
                    completed_through: id,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        fn notifications(&self) -> i64 {
            self.conn
                .query_row(
                    "SELECT count(*) FROM deliveries WHERE recipient_kind = 'human'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    #[test]
    fn holds_new_mail_until_explicit_resume_and_requires_turn_completion() {
        let mut b = Board::new();
        let old = b.send("work");
        let drain = b.drain("start");
        assert_eq!(drain.agents[0].unread, 2);
        assert!(drain.watermark > old);
        let held = b.send("after the watermark");
        assert!(held > drain.watermark);
        assert_eq!(b.drain("start").watermark, drain.watermark); // idempotent
        let mail = b.read();
        assert_eq!(mail.len(), 2);
        assert!(mail[1].body.contains("long-running background tasks"));
        assert_eq!(b.drain("").status, "draining"); // read != finished
        assert!(b.read().is_empty());
        let principal = db::Principal {
            kind: db::Kind::Agent,
            id: b.id,
            name: "worker".into(),
        };
        assert!(!db::has_unread(&b.conn, &principal).unwrap());
        b.complete(old); // stale completion cannot acknowledge the notice
        assert_eq!(b.drain("").status, "draining");
        b.complete(drain.watermark);
        assert_eq!(b.drain("").status, "ready");
        assert_eq!(b.notifications(), 1);
        b.complete(drain.watermark);
        b.drain("");
        assert_eq!(b.notifications(), 1);
        assert!(!b.drain("resume").frozen);
        assert_eq!(b.read()[0].id, held);
        assert_eq!(b.drain("resume").status, "running");
    }

    #[test]
    fn scoped_to_owner_and_agents_cannot_control_barriers() {
        let mut b = Board::new();
        let barrier = b.drain("start");
        assert!(
            agent_drain(
                &mut b.conn,
                &b.agent,
                proto::AgentDrainRequest {
                    action: "resume".into()
                }
            )
            .is_err()
        );
        assert!(
            report_drain(
                &mut b.conn,
                &b.owner,
                proto::ReportDrainRequest {
                    completed_through: barrier.watermark,
                    ..Default::default()
                }
            )
            .is_err()
        );
        let other = agent_drain(
            &mut b.conn,
            &b.other,
            proto::AgentDrainRequest {
                action: "resume".into(),
            },
        )
        .unwrap();
        assert!(!other.frozen);
        assert!(b.drain("").frozen);
        // Unowned-by-this-human agents continue receiving mail normally.
        let token = service::register(
            &mut b.conn,
            &b.other,
            proto::RegisterRequest {
                name: Some("other-worker".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        service::send(
            &mut b.conn,
            &b.other,
            proto::SendRequest {
                to: "other-worker".into(),
                body: "unaffected".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mail = service::read(
            &mut b.conn,
            &Caller {
                token: Some(token),
                ..Default::default()
            },
            proto::ReadRequest::default(),
        )
        .unwrap();
        assert_eq!(mail.messages.len(), 1);
    }

    #[test]
    fn timeout_notifies_without_browser_and_keeps_delivery_frozen() {
        let mut b = Board::new();
        b.drain("start");
        let later = Utc::now() + chrono::Duration::minutes(21);
        assert_eq!(check_agent_drains(&mut b.conn, later).unwrap(), 1);
        assert_eq!(check_agent_drains(&mut b.conn, later).unwrap(), 0);
        let drain = b.drain("");
        assert_eq!(drain.status, "timed_out");
        assert!(drain.frozen);
        assert!(!drain.agents[0].completed);
        assert_eq!(b.notifications(), 1);
        let held = b.send("held even after timeout");
        assert!(b.read().iter().all(|m| m.id != held));
    }

    #[test]
    fn mark_read_cannot_consume_held_mail_and_read_alone_is_insufficient() {
        let mut b = Board::new();
        let drain = b.drain("start");
        b.send("held");
        let marked = service::mark_read(
            &mut b.conn,
            &b.agent,
            proto::MarkReadRequest {
                from: "will".into(),
            },
        )
        .unwrap();
        assert_eq!(marked.count, 1);
        assert_eq!(b.drain("").status, "draining");
        b.complete(drain.watermark);
        assert_eq!(b.drain("").status, "ready");
        b.drain("resume");
        assert_eq!(b.read().len(), 1);
    }

    #[test]
    fn clock_changes_do_not_deliver_final_notice_ahead_of_older_mail() {
        let mut b = Board::new();
        let old = b.send("older id, future clock");
        b.conn
            .execute(
                "UPDATE messages SET created_at = '2099-01-01T00:00:00.000000Z' WHERE id = ?1",
                [old],
            )
            .unwrap();
        let drain = b.drain("start");
        let first = service::read(
            &mut b.conn,
            &b.agent,
            proto::ReadRequest {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.messages[0].id, old);
        b.complete(old);
        assert_eq!(b.drain("").status, "draining");
        assert_eq!(b.read()[0].id, drain.watermark);
        b.complete(drain.watermark);
        assert_eq!(b.drain("").status, "ready");
    }

    #[test]
    fn side_questions_delay_readiness_and_new_side_turns_are_frozen() {
        let mut b = Board::new();
        b.conn.execute("INSERT INTO side_requests(agent_id, question, asked_at, status, updated_at) VALUES (?1, 'active side turn', 'now', 'open', 'now')", [b.id]).unwrap();
        let first = b.drain("start");
        b.read();
        b.complete(first.watermark);
        let status = b.drain("");
        assert_eq!(status.status, "draining");
        assert!(status.agents[0].completed);
        assert!(status.agents[0].side_pending);
        let error = service::ask_side(
            &mut b.conn,
            &b.owner,
            proto::AskSideRequest {
                agent: "worker".into(),
                question: "new side turn".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("paused for restart"));
        b.conn
            .execute("UPDATE side_requests SET status = 'cancelled'", [])
            .unwrap();
        // Owner cancellation cannot stand in for a stopped provider turn.
        assert_eq!(b.drain("").status, "draining");
        let side_id = b
            .conn
            .query_row("SELECT max(id) FROM side_requests", [], |r| r.get(0))
            .unwrap();
        report_drain(
            &mut b.conn,
            &b.agent,
            proto::ReportDrainRequest {
                completed_through: first.watermark,
                side_completed_through: side_id,
            },
        )
        .unwrap();
        assert_eq!(b.drain("").status, "ready");
    }

    #[test]
    fn barrier_and_notification_survive_reopening_the_database() {
        let mut b = Board::new();
        let first = b.drain("start");
        let held = b.send("queued across restart");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bbs.db");
        b.conn
            .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        b.conn = Connection::open(&path).unwrap();
        db::migrate(&mut b.conn).unwrap();
        assert_eq!(b.drain("start").watermark, first.watermark);
        assert_eq!(b.read().len(), 1);
        b.complete(first.watermark);
        b.conn = Connection::open(&path).unwrap();
        assert_eq!(b.drain("").status, "ready");
        assert_eq!(b.notifications(), 1);
        assert!(b.read().is_empty());
        b.drain("resume");
        assert_eq!(b.read()[0].id, held);
    }

    #[test]
    fn empty_barrier_is_ready_and_a_second_drain_needs_a_fresh_ack() {
        let mut b = Board::new();
        let first = b.drain("start");
        b.read();
        b.complete(first.watermark);
        b.drain("resume");
        let second = b.drain("start");
        b.read();
        b.complete(first.watermark);
        assert_eq!(b.drain("").status, "draining");
        b.complete(second.watermark);
        assert_eq!(b.drain("").status, "ready");
        let empty = agent_drain(
            &mut b.conn,
            &b.other,
            proto::AgentDrainRequest {
                action: "start".into(),
            },
        )
        .unwrap();
        assert_eq!(empty.status, "ready");
        assert!(empty.agents.is_empty());
    }
}
