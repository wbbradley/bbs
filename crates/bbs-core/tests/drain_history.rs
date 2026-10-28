//! Restart drains distinguish completed historical side work from work that
//! still needs the current runner to acknowledge provider quiescence.
use bbs_core::{db, proto, service};
use rusqlite::Connection;

struct Board {
    conn: Connection,
    owner: service::Caller,
    agent: service::Caller,
}
impl Board {
    fn new() -> Self {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "tester".into(),
            },
        )
        .unwrap();
        let owner = service::Caller {
            human: Some("tester".into()),
            ..Default::default()
        };
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
        let agent = service::Caller {
            token: Some(token),
            ..Default::default()
        };
        Self { conn, owner, agent }
    }
    fn session(&mut self, generation: &str, supported: bool) {
        service::begin_side_session(
            &mut self.conn,
            &self.agent,
            proto::BeginSideSessionRequest {
                generation: generation.into(),
            },
        )
        .unwrap();
        service::report_context(
            &mut self.conn,
            &self.agent,
            proto::ReportContextRequest {
                generation: generation.into(),
                side_supported: Some(supported),
                side_followups: Some(supported),
                preserve_usage: true,
                ..Default::default()
            },
        )
        .unwrap();
    }
    fn claimed(&mut self, generation: &str) -> i64 {
        let id = service::ask_side(
            &mut self.conn,
            &self.owner,
            proto::AskSideRequest {
                agent: "worker".into(),
                question: "question".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .id;
        service::claim_side(
            &mut self.conn,
            &self.agent,
            proto::ClaimSideRequest {
                generation: generation.into(),
            },
        )
        .unwrap();
        id
    }
    fn start_and_complete_parent(&mut self) -> proto::AgentDrainResponse {
        let start = service::agent_drain(
            &mut self.conn,
            &self.owner,
            proto::AgentDrainRequest {
                action: "start".into(),
            },
        )
        .unwrap();
        service::read(&mut self.conn, &self.agent, proto::ReadRequest::default()).unwrap();
        service::report_drain(
            &mut self.conn,
            &self.agent,
            proto::ReportDrainRequest {
                completed_through: start.watermark,
                side_completed_through: 0,
            },
        )
        .unwrap();
        service::agent_drain(
            &mut self.conn,
            &self.owner,
            proto::AgentDrainRequest::default(),
        )
        .unwrap()
    }
}

#[test]
fn historical_completed_ended_question_does_not_block_new_unsupported_runner() {
    let mut b = Board::new();
    b.session("codex-generation", true);
    let id = b.claimed("codex-generation");
    service::report_side(
        &mut b.conn,
        &b.agent,
        proto::ReportSideRequest {
            generation: "codex-generation".into(),
            id,
            status: proto::SideStatus::SideCompleted.into(),
            answer: Some("finished".into()),
        },
    )
    .unwrap();
    service::cancel_side(
        &mut b.conn,
        &b.owner,
        proto::CancelSideRequest { id, end: true },
    )
    .unwrap();
    b.session("acp-generation", false);
    let result = b.start_and_complete_parent();
    assert!(result.agents[0].completed);
    assert_eq!(result.agents[0].unread, 0);
    assert!(!result.agents[0].side_pending);
    assert_eq!(result.status, "ready");
}

#[test]
fn cancelled_current_generation_remains_pending_without_provider_ack() {
    let mut b = Board::new();
    b.session("codex-generation", true);
    let id = b.claimed("codex-generation");
    service::cancel_side(
        &mut b.conn,
        &b.owner,
        proto::CancelSideRequest { id, end: true },
    )
    .unwrap();
    let result = b.start_and_complete_parent();
    assert!(result.agents[0].side_pending);
    assert_eq!(result.status, "draining");
}

#[test]
fn only_completed_claimed_history_from_a_different_generation_is_exempt() {
    for status in [
        "open",
        "completed",
        "failed",
        "cancelled",
        "interrupted",
        "ended",
    ] {
        for previous in [false, true] {
            for claimed in [false, true] {
                let mut b = Board::new();
                b.session("old-generation", true);
                let id = b.claimed("old-generation");
                if previous {
                    b.session("current-generation", false);
                } else {
                    // An unavailable driver in the SAME generation may mean
                    // cleanup failed. It must never exempt that generation.
                    service::report_context(
                        &mut b.conn,
                        &b.agent,
                        proto::ReportContextRequest {
                            generation: "old-generation".into(),
                            side_supported: Some(false),
                            preserve_usage: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                b.conn.execute("UPDATE side_requests SET status = ?1, ended = 1, generation = CASE WHEN ?2 THEN generation ELSE NULL END WHERE id = ?3", rusqlite::params![status, claimed, id]).unwrap();
                let state = b.start_and_complete_parent();
                let exempt = previous && claimed && status == "completed";
                assert_eq!(
                    state.agents[0].side_pending, !exempt,
                    "status={status}, previous={previous}, claimed={claimed}"
                );
                assert_eq!(state.status, if exempt { "ready" } else { "draining" });
            }
        }
    }
}

#[test]
fn completed_history_without_a_current_session_cannot_imply_quiescence() {
    let mut b = Board::new();
    b.session("old-generation", true);
    let id = b.claimed("old-generation");
    service::report_side(
        &mut b.conn,
        &b.agent,
        proto::ReportSideRequest {
            generation: "old-generation".into(),
            id,
            status: proto::SideStatus::SideCompleted.into(),
            ..Default::default()
        },
    )
    .unwrap();
    b.conn.execute("DELETE FROM side_state", []).unwrap();
    let state = b.start_and_complete_parent();
    assert!(state.agents[0].side_pending);
    assert_eq!(state.status, "draining");
}

#[test]
fn history_reconciliation_notifies_once_and_keeps_held_mail_paused() {
    let mut b = Board::new();
    b.session("old-generation", true);
    let id = b.claimed("old-generation");
    service::report_side(
        &mut b.conn,
        &b.agent,
        proto::ReportSideRequest {
            generation: "old-generation".into(),
            id,
            status: proto::SideStatus::SideCompleted.into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(b.start_and_complete_parent().status, "draining");
    b.session("acp-generation", false);
    let (state, changed) = service::agent_drain_with_changes(
        &mut b.conn,
        &b.owner,
        proto::AgentDrainRequest::default(),
    )
    .unwrap();
    assert_eq!(state.status, "ready");
    assert!(state.frozen);
    assert!(changed);
    let (_, changed) = service::agent_drain_with_changes(
        &mut b.conn,
        &b.owner,
        proto::AgentDrainRequest::default(),
    )
    .unwrap();
    assert!(!changed);
    let notices: i64 = b
        .conn
        .query_row(
            "SELECT count(*) FROM deliveries WHERE recipient_kind = 'human'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1);
    service::send(
        &mut b.conn,
        &b.owner,
        proto::SendRequest {
            to: "worker".into(),
            body: "held".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        service::read(&mut b.conn, &b.agent, proto::ReadRequest::default())
            .unwrap()
            .messages
            .is_empty()
    );
}
