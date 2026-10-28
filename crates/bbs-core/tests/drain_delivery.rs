//! Restart barriers retain ordinary work while preserving owner intervention
//! and keeping every frozen agent visible in the captured cohort.
use bbs_core::{db, proto, service};
use rusqlite::Connection;

struct Board {
    conn: Connection,
    owner: service::Caller,
    other: service::Caller,
    agent: service::Caller,
    id: i64,
}

fn human(conn: &mut Connection, name: &str) -> service::Caller {
    service::ensure_human(conn, proto::EnsureHumanRequest { name: name.into() }).unwrap();
    service::Caller {
        human: Some(name.into()),
        ..Default::default()
    }
}

fn register(
    conn: &mut Connection,
    owner: &service::Caller,
    name: Option<&str>,
) -> anyhow::Result<proto::RegisterResponse> {
    service::register(
        conn,
        owner,
        proto::RegisterRequest {
            name: name.map(str::to_owned),
            ..Default::default()
        },
    )
}

impl Board {
    fn new() -> Self {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let owner = human(&mut conn, "owner");
        let other = human(&mut conn, "other");
        let registered = register(&mut conn, &owner, Some("worker")).unwrap();
        let agent = service::Caller {
            token: Some(registered.token),
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
        service::agent_drain(
            &mut self.conn,
            &self.owner,
            proto::AgentDrainRequest {
                action: action.into(),
            },
        )
        .unwrap()
    }
    fn send(&mut self, body: &str, control: Option<proto::Control>) -> i64 {
        service::send(
            &mut self.conn,
            &self.owner,
            proto::SendRequest {
                to: "worker".into(),
                body: body.into(),
                control: control.map(Into::into),
            },
        )
        .unwrap()
        .id
        .unwrap()
    }
    fn read(&mut self, limit: Option<u32>) -> Vec<proto::Message> {
        service::read(
            &mut self.conn,
            &self.agent,
            proto::ReadRequest {
                limit,
                ..Default::default()
            },
        )
        .unwrap()
        .messages
    }
    fn has_unread(&self) -> bool {
        db::has_unread(
            &self.conn,
            &db::Principal {
                kind: db::Kind::Agent,
                id: self.id,
                name: "worker".into(),
            },
        )
        .unwrap()
    }
    fn reject_new_registration(&mut self) {
        for name in [Some("newcomer"), None] {
            let error = register(&mut self.conn, &self.owner, name).unwrap_err();
            assert!(
                error.to_string().contains("resume delivery in Settings"),
                "{error:#}"
            );
        }
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[test]
fn owner_cancel_bypasses_drain_without_releasing_work_or_other_controls() {
    let mut b = Board::new();
    let barrier = b.drain("start");
    let work = b.send("held ordinary work", None);
    let effort = b.send("high", Some(proto::Control::SetEffort));
    let cancel = b.send("cancel", Some(proto::Control::Cancel));
    // Even a backwards wall clock cannot let a newer control overtake the
    // final notice, which establishes the runner's completion watermark.
    b.conn
        .execute(
            "UPDATE messages SET created_at = '2000-01-01T00:00:00.000000Z' WHERE id = ?1",
            [cancel],
        )
        .unwrap();
    assert_eq!(b.read(Some(1))[0].id, barrier.watermark);
    assert!(
        b.has_unread(),
        "the local waiting reader must notice owner Cancel"
    );
    let messages = b.read(None);
    assert_eq!(
        messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![cancel]
    );
    assert_eq!(messages[0].control(), proto::Control::Cancel);
    assert!(!b.has_unread(), "only held mail remains");
    assert!(b.read(None).is_empty());
    assert!(b.drain("").frozen);
    b.drain("resume");
    assert_eq!(
        b.read(None).iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![work, effort]
    );
}

#[test]
fn marking_owner_mail_read_includes_cancel_but_leaves_held_mail_unread() {
    let mut b = Board::new();
    b.drain("start");
    b.read(None);
    let work = b.send("held", None);
    let effort = b.send("high", Some(proto::Control::SetEffort));
    b.send("cancel", Some(proto::Control::Cancel));
    assert!(b.has_unread());
    let result = service::mark_read(
        &mut b.conn,
        &b.agent,
        proto::MarkReadRequest {
            from: "owner".into(),
        },
    )
    .unwrap();
    assert_eq!(result.count, 1);
    assert!(!b.has_unread());
    b.drain("resume");
    assert_eq!(
        b.read(None).iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![work, effort]
    );
}

#[test]
fn cancellation_still_requires_the_owner_while_frozen() {
    let mut b = Board::new();
    b.drain("start");
    for caller in [&b.other, &b.agent] {
        let result = service::send(
            &mut b.conn,
            caller,
            proto::SendRequest {
                to: "worker".into(),
                body: "cancel".into(),
                control: Some(proto::Control::Cancel.into()),
            },
        );
        assert!(result.is_err());
    }
}

#[test]
fn new_registration_is_refused_during_drain_ready_and_timeout_until_resume() {
    let mut b = Board::new();
    b.drain("start");
    b.reject_new_registration();
    // Completing or timing out the assessment does not unfreeze its owner.
    for status in ["ready", "timed_out"] {
        b.conn
            .execute("UPDATE agent_drains SET status = ?1", [status])
            .unwrap();
        b.reject_new_registration();
    }
    // Other owners are not part of this freeze.
    register(&mut b.conn, &b.other, Some("unaffected")).unwrap();
    b.drain("resume");
    register(&mut b.conn, &b.owner, Some("newcomer")).unwrap();
}

#[test]
fn existing_agent_takeover_remains_available_during_the_freeze() {
    let mut b = Board::new();
    b.drain("start");
    let error = register(&mut b.conn, &b.owner, Some("worker")).unwrap_err();
    assert!(error.to_string().contains("already reserved"));
    let rotated = service::rotate_agent_token(
        &mut b.conn,
        &b.owner,
        proto::RetireRequest {
            agent: "worker".into(),
        },
    )
    .unwrap();
    let restarted = service::Caller {
        token: Some(rotated.token),
        ..Default::default()
    };
    assert_eq!(service::whoami(&mut b.conn, &restarted).unwrap().id, b.id);
    assert_eq!(b.drain("").agents.len(), 1);
}

#[test]
fn empty_ready_barrier_refuses_new_agents_and_retired_names_stay_blocked() {
    let mut b = Board::new();
    service::retire_agent(
        &mut b.conn,
        &b.owner,
        proto::RetireRequest {
            agent: "worker".into(),
        },
    )
    .unwrap();
    let empty = b.drain("start");
    assert_eq!(empty.status, "ready");
    assert!(empty.agents.is_empty());
    for name in ["worker", "newcomer"] {
        let error = register(&mut b.conn, &b.owner, Some(name)).unwrap_err();
        assert!(error.to_string().contains("resume delivery in Settings"));
    }
    b.drain("resume");
    register(&mut b.conn, &b.owner, Some("worker")).unwrap();
}
