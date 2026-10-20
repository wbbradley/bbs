//! Optional native human input and context reports, independent of chat/activity.
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};

use super::harness::{Event, Harness};
use super::input::Form;
use crate::backend::{AGENT_INTERACTIONS, Backend};
use crate::config::Config;
use crate::proto::{self, AnswerAction, InputStatus};
use crate::service::Caller;

const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PENDING: usize = 32;

pub struct Session {
    backend: Backend,
    generation: String,
    #[cfg(test)]
    publish_ack: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<bool>,
    )>,
}
impl Session {
    pub async fn begin(config: &Config, dir: &Path, token: &str) -> Result<Option<Self>> {
        timeout(CALL_TIMEOUT, async {
            let mut backend = Backend::open(
                config,
                dir,
                Caller {
                    token: Some(token.into()),
                    ..Default::default()
                },
            )
            .await?;
            if !backend
                .server_info()
                .await?
                .capabilities
                .iter()
                .any(|c| c == AGENT_INTERACTIONS)
            {
                return Ok(None);
            }
            let generation = crate::auth::new_token();
            backend
                .begin_side_session(proto::BeginSideSessionRequest {
                    generation: generation.clone(),
                })
                .await?;
            Ok(Some(Self {
                backend,
                generation,
                #[cfg(test)]
                publish_ack: None,
            }))
        })
        .await?
    }

    pub fn start(self, harness: Harness, warnings: mpsc::UnboundedSender<String>) -> Side {
        let (commands, rx) = mpsc::channel(MAX_PENDING);
        let (usage, usage_rx) = watch::channel(None);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            self,
            harness.clone(),
            warnings.clone(),
            rx,
            usage_rx,
            stop_rx,
        ));
        Side {
            commands,
            usage,
            stop,
            task,
            harness,
            warnings,
        }
    }
}

enum Command {
    Open(Form),
    Close(String),
}
#[derive(Clone)]
struct Snapshot {
    usage: Option<proto::ContextUsage>,
}
struct Pending {
    form: Form,
    seq: i64,
    id: Option<i64>,
    closed: bool,
}

pub struct Side {
    commands: mpsc::Sender<Command>,
    usage: watch::Sender<Option<Snapshot>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    harness: Harness,
    warnings: mpsc::UnboundedSender<String>,
}
impl Side {
    pub fn event(&self, event: &Event) {
        let command = match event {
            Event::OwnerInput(f) => Some(Command::Open(f.clone())),
            Event::InputClosed(key) => Some(Command::Close(key.clone())),
            Event::ContextChanged(usage) => {
                let _ = self.usage.send(Some(Snapshot { usage: *usage }));
                None
            }
            _ => None,
        };
        if let Some(command) = command
            && self.commands.try_send(command).is_err()
        {
            self.harness.enable_input(false);
            self.harness.cancel_input();
            let _ = self.stop.send(true);
            let _ = self
                .warnings
                .send("human input disabled: its bounded delivery queue is full or stopped".into());
        }
    }
    pub async fn finish(mut self) {
        self.harness.enable_input(false);
        self.harness.cancel_input();
        let _ = self.stop.send(true);
        if timeout(Duration::from_secs(4), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = self.warnings.send(
                "human-input cleanup incomplete; the next runner session will retire old requests"
                    .into(),
            );
        }
    }
}

fn command(c: Command, queue: &mut VecDeque<Pending>, seq: &mut i64) {
    match c {
        Command::Open(form) => {
            *seq += 1;
            queue.push_back(Pending {
                form,
                seq: *seq,
                id: None,
                closed: false,
            });
        }
        Command::Close(key) => {
            if let Some(p) = queue.iter_mut().find(|p| p.form.key == key) {
                p.closed = true;
            }
        }
    }
}

async fn run(
    mut session: Session,
    harness: Harness,
    warnings: mpsc::UnboundedSender<String>,
    mut commands: mpsc::Receiver<Command>,
    mut usage: watch::Receiver<Option<Snapshot>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut queue = VecDeque::new();
    let mut seq = 0;
    let mut warned = false;
    let mut ending = false;
    let mut next_input_at = Instant::now();
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if ending && queue.is_empty() {
            break;
        }
        let mut context = None;
        tokio::select! {
            biased;
            changed = stop.changed(), if !ending => { if changed.is_err() || *stop.borrow() { ending = true; } }
            c = commands.recv(), if !ending => match c { Some(c) => command(c, &mut queue, &mut seq), None => ending = true },
            changed = usage.changed(), if !ending => { if changed.is_ok() { context = usage.borrow_and_update().clone(); } }
            _ = tick.tick(), if !queue.is_empty() => {}
        }
        while let Ok(c) = commands.try_recv() {
            command(c, &mut queue, &mut seq);
        }
        if queue.len() > MAX_PENDING {
            ending = true;
        }
        for p in &mut queue {
            p.closed |= ending || !harness.input_pending(&p.form.key);
        }
        if ending {
            harness.enable_input(false);
            harness.cancel_input();
        }
        if ending && queue.is_empty() {
            break;
        }

        // Context snapshots are coalesced, with no periodic writes/retry loops.
        if let Some(snapshot) = context {
            let result = timeout(
                CALL_TIMEOUT,
                session.backend.report_context(proto::ReportContextRequest {
                    generation: session.generation.clone(),
                    usage: snapshot.usage,
                    side_supported: None,
                    side_reason: None,
                }),
            )
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
            if let Err(error) = result {
                warn(&warnings, &mut warned, &error);
                if permanent(&session.backend, &error) {
                    break;
                }
            } else {
                warned = false;
            }
        }
        if !ending && Instant::now() < next_input_at {
            continue;
        }
        let Some(p) = queue.front_mut() else {
            continue;
        };
        // Recheck after awaited I/O: cancellation never depends on actor scheduling.
        p.closed |= ending || !harness.input_pending(&p.form.key);
        let closing = p.closed;
        let result: Result<()> = if closing {
            timeout(
                CALL_TIMEOUT,
                session
                    .backend
                    .close_owner_input(proto::CloseOwnerInputRequest {
                        generation: session.generation.clone(),
                        request_id: None,
                        request_seq: Some(p.seq),
                    }),
            )
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .map(|_| ())
        } else if p.id.is_none() {
            let result = timeout(
                CALL_TIMEOUT,
                session
                    .backend
                    .request_owner_input(proto::RequestOwnerInputRequest {
                        generation: session.generation.clone(),
                        request_seq: p.seq,
                        questions: p.form.questions.clone(),
                        message: p.form.message.clone(),
                        is_blocking: p.form.blocking,
                    }),
            )
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
            #[cfg(test)]
            let result = if let Some((reached, release)) = session.publish_ack.take() {
                let _ = reached.send(());
                if release.await.unwrap_or(true) {
                    result
                } else {
                    Err(tonic::Status::unavailable("synthetic lost publish acknowledgment").into())
                }
            } else {
                result
            };
            result.map(|r| p.id = Some(r.request_id))
        } else {
            let result = timeout(
                CALL_TIMEOUT,
                session.backend.side_report(proto::SideReportRequest {
                    agent: String::new(),
                    generation: Some(session.generation.clone()),
                }),
            )
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
            match result {
                Err(e) => Err(e),
                Ok(report) => {
                    if let Some(answer) =
                        report.pending_input.filter(|a| Some(a.request_id) == p.id)
                    {
                        let action = match answer.status() {
                            InputStatus::Answered => Some(AnswerAction::Submit),
                            InputStatus::Declined => Some(AnswerAction::Decline),
                            InputStatus::Cancelled | InputStatus::Stale => {
                                Some(AnswerAction::Cancel)
                            }
                            _ => None,
                        };
                        if let Some(action) = action {
                            // The provider manager takes the exact pending key before
                            // writing. A withdrawal or newer callback makes this inert.
                            let response = timeout(
                                CALL_TIMEOUT,
                                harness.answer_input(&p.form.key, action, &answer.answers),
                            )
                            .await;
                            if !matches!(response, Ok(Ok(()))) {
                                let _ = warnings.send("native human-input response could not be delivered; no retry or prompt was submitted".into());
                            }
                            p.closed = true;
                        }
                    }
                    Ok(())
                }
            }
        };
        next_input_at = Instant::now() + Duration::from_secs(1);
        match result {
            Ok(()) => {
                warned = false;
                if closing {
                    queue.pop_front();
                    if !ending && queue.is_empty() {
                        harness.enable_input(true);
                    }
                }
            }
            Err(error) => {
                warn(&warnings, &mut warned, &error);
                harness.enable_input(false);
                harness.cancel_input();
                for p in &mut queue {
                    p.closed = true;
                }
                if ending || permanent(&session.backend, &error) {
                    break;
                }
            }
        }
    }
    harness.enable_input(false);
    harness.cancel_input();
}

fn warn(warnings: &mpsc::UnboundedSender<String>, warned: &mut bool, error: &anyhow::Error) {
    if !*warned {
        let _ = warnings.send(format!(
            "human input/context reporting unavailable: {error:#}"
        ));
        *warned = true;
    }
}
fn permanent(backend: &Backend, error: &anyhow::Error) -> bool {
    if let Some(status) = error.downcast_ref::<tonic::Status>() {
        return matches!(
            status.code(),
            tonic::Code::InvalidArgument
                | tonic::Code::PermissionDenied
                | tonic::Code::Unauthenticated
                | tonic::Code::FailedPrecondition
                | tonic::Code::Unimplemented
                | tonic::Code::NotFound
        );
    }
    if matches!(backend, Backend::Local(_)) {
        return !matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
    }
    error
        .downcast_ref::<crate::auth::AuthenticationError>()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::super::{
        acp::McpServer,
        harness::BbsTools,
        mcp::Tools,
        rpc::Process,
        settings::{HarnessConfig, HarnessType},
    };
    use super::*;
    async fn provider(
        dir: &Path,
        kind: HarnessType,
        enabled: bool,
    ) -> (Harness, Process, mpsc::UnboundedReceiver<Event>) {
        let name = if kind == HarnessType::Acp {
            "acp"
        } else {
            "codex"
        };
        let config = HarnessConfig {
            kind,
            command: vec![
                "python3".into(),
                format!("{}/tests/fake_interactions.py", env!("CARGO_MANIFEST_DIR")),
                name.into(),
                if enabled { "normal" } else { "disabled" }.into(),
            ],
            ..Default::default()
        };
        let bbs = BbsTools {
            mcp: McpServer {
                name: "test".into(),
                command: "unused".into(),
                args: vec![],
                env: vec![],
            },
            tools: Tools::new(dir, "synthetic".into()),
            input_enabled: enabled,
        };
        let (h, p, e, _) = Harness::start(&config, dir, &dir.join("stderr"), bbs, None)
            .await
            .unwrap();
        (h, p, e)
    }
    async fn board(dir: &Path) -> (Session, Backend, String) {
        let config = Config::default();
        let mut db = crate::db::open(&dir.join("bbs.db")).unwrap();
        crate::db::migrate(&mut db).unwrap();
        drop(db);
        let mut human = Backend::open(
            &config,
            dir,
            Caller {
                human: Some("tester".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        human
            .ensure_human(proto::EnsureHumanRequest {
                name: "tester".into(),
            })
            .await
            .unwrap();
        let agent = human
            .register(proto::RegisterRequest {
                name: Some("NativeTest".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::begin(&config, dir, &agent.token)
            .await
            .unwrap()
            .unwrap();
        (session, human, agent.token)
    }
    async fn form(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(e @ Event::OwnerInput(_)) = events.recv().await {
                    return e;
                }
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn cancelled_before_publish_ack_closes_by_sequence_without_answering_twice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, mut human, _) = board(dir.path()).await;
        let (reached, ack) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        session.publish_ack = Some((reached, wait));
        let (harness, mut process, mut events) =
            provider(dir.path(), HarnessType::CodexAppServer, true).await;
        let (warnings, _) = mpsc::unbounded_channel();
        let side = session.start(harness.clone(), warnings);
        let h = harness.clone();
        let prompt = tokio::spawn(async move { h.prompt("ASK", None).await });
        side.event(&form(&mut events).await);
        timeout(Duration::from_secs(5), ack).await.unwrap().unwrap();
        let report = human
            .side_report(proto::SideReportRequest {
                agent: "NativeTest".into(),
                generation: None,
            })
            .await
            .unwrap();
        assert_eq!(report.pending_input.unwrap().status(), InputStatus::Pending);
        harness.cancel_input();
        timeout(Duration::from_secs(5), async {
            loop {
                let wire = std::fs::read_to_string(dir.path().join("interaction-wire.jsonl"))
                    .unwrap_or_default();
                if wire
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .any(|v| v["id"] == "question")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.send(true).unwrap();
        side.finish().await;
        assert_eq!(
            human
                .side_report(proto::SideReportRequest {
                    agent: "NativeTest".into(),
                    generation: None
                })
                .await
                .unwrap()
                .pending_input
                .unwrap()
                .status(),
            InputStatus::Cancelled
        );
        timeout(Duration::from_secs(5), prompt)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        process.stop().await;
        let wire = std::fs::read_to_string(dir.path().join("interaction-wire.jsonl")).unwrap();
        let answers: Vec<serde_json::Value> = wire
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .filter(|v: &serde_json::Value| v["id"] == "question")
            .collect();
        assert_eq!(
            wire.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v["kind"] == "prompt")
                .count(),
            1
        );
        assert_eq!(answers.len(), 1);
        assert_eq!(
            answers[0]["response"]["result"],
            serde_json::json!({"answers":{}})
        );
    }
    #[tokio::test]
    async fn lost_publish_ack_cancels_then_cleans_and_accepts_a_later_form() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, mut human, _) = board(dir.path()).await;
        let (reached, ack) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        session.publish_ack = Some((reached, wait));
        let (harness, mut process, mut events) =
            provider(dir.path(), HarnessType::CodexAppServer, true).await;
        let (warnings, mut warning_rx) = mpsc::unbounded_channel();
        let side = session.start(harness.clone(), warnings);
        let h = harness.clone();
        let prompt = tokio::spawn(async move { h.prompt("ASK", None).await });
        side.event(&form(&mut events).await);
        ack.await.unwrap();
        release.send(false).unwrap();
        timeout(Duration::from_secs(5), warning_rx.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), prompt)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                let r = human
                    .side_report(proto::SideReportRequest {
                        agent: "NativeTest".into(),
                        generation: None,
                    })
                    .await
                    .unwrap();
                if r.pending_input
                    .is_some_and(|i| i.status() == InputStatus::Cancelled)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let h = harness.clone();
        let prompt = tokio::spawn(async move { h.prompt("LATER", None).await });
        let e = form(&mut events).await;
        assert!(matches!(e, Event::OwnerInput(_)));
        side.event(&e);
        harness.cancel_input();
        side.finish().await;
        timeout(Duration::from_secs(5), prompt)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        process.stop().await;
        assert!(
            warning_rx.try_recv().is_err(),
            "one warning per outage, no retry storm"
        );
    }

    #[tokio::test]
    async fn stale_generation_cancels_input_without_reclaiming_or_stopping_parent() {
        let dir = tempfile::tempdir().unwrap();
        let (session, mut human, token) = board(dir.path()).await;
        let (harness, mut process, mut events) =
            provider(dir.path(), HarnessType::CodexAppServer, true).await;
        let (warnings, mut warnings_rx) = mpsc::unbounded_channel();
        let side = session.start(harness.clone(), warnings);
        let h = harness.clone();
        let prompt = tokio::spawn(async move { h.prompt("ASK", None).await });
        side.event(&form(&mut events).await);
        // A newer process generation wins; the old actor must never begin again.
        let mut newer = Backend::open(
            &Config::default(),
            dir.path(),
            Caller {
                token: Some(token),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        newer
            .begin_side_session(proto::BeginSideSessionRequest {
                generation: "newer-process".into(),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(5), warnings_rx.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), prompt)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        harness.prompt("ORDINARY NEXT TURN", None).await.unwrap();
        assert!(
            newer
                .side_report(proto::SideReportRequest {
                    agent: String::new(),
                    generation: Some("newer-process".into())
                })
                .await
                .is_ok()
        );
        assert!(
            human
                .side_report(proto::SideReportRequest {
                    agent: "NativeTest".into(),
                    generation: None
                })
                .await
                .unwrap()
                .pending_input
                .is_none_or(|i| i.status() != InputStatus::Pending)
        );
        side.finish().await;
        process.stop().await;
    }
    #[derive(Clone)]
    struct OldServer;
    impl tonic::server::NamedService for OldServer {
        const NAME: &'static str = "bbs.v2.Bbs";
    }
    impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for OldServer {
        type Response = tonic::codegen::http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
            assert_eq!(
                req.uri().path(),
                "/bbs.v2.Bbs/ServerInfo",
                "old server must never receive optional feature calls"
            );
            struct Info;
            impl tonic::server::UnaryService<proto::Empty> for Info {
                type Response = proto::ServerInfoResponse;
                type Future =
                    tonic::codegen::BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                fn call(&mut self, _: tonic::Request<proto::Empty>) -> Self::Future {
                    Box::pin(async {
                        let mut info = crate::protocol::Policy::default().info(false);
                        info.capabilities.retain(|c| c != AGENT_INTERACTIONS);
                        Ok(tonic::Response::new(info))
                    })
                }
            }
            Box::pin(async {
                Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                    .unary(Info, req)
                    .await)
            })
        }
    }
    #[tokio::test]
    async fn disabled_optional_features_keep_ordinary_provider_turns_working() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            server: Some(listener.local_addr().unwrap().to_string()),
            ..Default::default()
        };
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(OldServer)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let scratch = tempfile::tempdir().unwrap();
        assert!(
            Session::begin(&config, scratch.path(), "synthetic")
                .await
                .unwrap()
                .is_none()
        );
        server.abort();
        for kind in [HarnessType::Acp, HarnessType::CodexAppServer] {
            let dir = tempfile::tempdir().unwrap();
            let (harness, mut process, mut events) = provider(dir.path(), kind, false).await;
            timeout(Duration::from_secs(5), harness.prompt("ASK", None))
                .await
                .unwrap()
                .unwrap();
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event, Event::OwnerInput(_)));
            }
            timeout(Duration::from_secs(5), harness.prompt("NEXT", None))
                .await
                .unwrap()
                .unwrap();
            process.stop().await;
        }
    }
}
