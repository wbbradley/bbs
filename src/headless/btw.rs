//! Board lifecycle for one side request, independent of parent mail and metadata.
use super::{
    codex_btw::{Mode, Outcome, Status},
    harness::Harness,
};
use crate::{
    backend::{Backend, SIDE_CONVERSATIONS},
    proto::{self, SideStatus},
};
use anyhow::Result;
use std::time::Duration;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, timeout},
};

const CALL: Duration = Duration::from_secs(3);
const IDLE: Duration = Duration::from_secs(31 * 60);
#[derive(Default)]
struct Hooks {
    #[cfg(test)]
    claim_ack: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<bool>,
    )>,
}
pub struct Btw {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    harness: Harness,
    warnings: mpsc::UnboundedSender<String>,
}
struct Active {
    id: i64,
    conversation: i64,
    ending: bool,
    cancel: watch::Sender<bool>,
    task: JoinHandle<Outcome>,
    reported: String,
}
impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}

struct Held {
    conversation: i64,
    latest: i64,
    deadline: Instant,
}
impl Held {
    fn follows(&self, request: &proto::SideRequest) -> bool {
        request.conversation == Some(self.conversation)
            && request.follows == Some(self.latest)
            && Instant::now() < self.deadline
    }
}

struct Final {
    id: i64,
    status: SideStatus,
    answer: Option<String>,
}

impl Btw {
    pub async fn start(
        mut backend: Backend,
        generation: String,
        harness: Harness,
        warnings: mpsc::UnboundedSender<String>,
    ) -> Option<Self> {
        let followups = timeout(CALL, backend.server_info())
            .await
            .ok()
            .and_then(Result::ok)
            .is_some_and(|r| r.capabilities.iter().any(|c| c == SIDE_CONVERSATIONS));
        let supported = harness.side_supported();
        if availability(
            &mut backend,
            &generation,
            supported,
            followups.then_some(supported),
            if supported {
                "Codex ephemeral fork is required; shared tools are allowed"
            } else {
                "ACP side-fork isolation has not been verified"
            },
        )
        .await
        .is_err()
        {
            let _ = warnings.send(
                "side-question capability reporting unavailable; ordinary turns continue".into(),
            );
            return None;
        }
        if !supported {
            return None;
        }
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(run(
            backend,
            generation,
            harness.clone(),
            warnings.clone(),
            rx,
            Hooks::default(),
            followups,
        ));
        Some(Self {
            stop,
            task,
            harness,
            warnings,
        })
    }
    pub async fn finish(mut self) {
        let _ = self.stop.send(true);
        if timeout(Duration::from_secs(15), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        if !self.harness.finish_side().await {
            let _ = self
                .warnings
                .send("side child cleanup unconfirmed; shared tools may already have run".into());
        }
    }
}
async fn availability(
    b: &mut Backend,
    g: &str,
    supported: bool,
    followups: Option<bool>,
    reason: &str,
) -> Result<()> {
    timeout(
        CALL,
        b.report_context(proto::ReportContextRequest {
            side_followups: followups,
            generation: g.into(),
            usage: None,
            side_supported: Some(supported),
            side_reason: Some(reason.into()),
            preserve_usage: true,
        }),
    )
    .await??;
    Ok(())
}
async fn snapshot(b: &mut Backend, g: &str) -> Result<proto::SideReportResponse> {
    timeout(
        CALL,
        b.side_report(proto::SideReportRequest {
            agent: String::new(),
            generation: Some(g.into()),
        }),
    )
    .await?
}
async fn report(b: &mut Backend, g: &str, f: &Final) -> Result<()> {
    timeout(
        CALL,
        b.report_side(proto::ReportSideRequest {
            generation: g.into(),
            id: f.id,
            status: f.status.into(),
            answer: f.answer.clone(),
        }),
    )
    .await??;
    Ok(())
}
fn bounded_detail(mut text: String) -> String {
    let mut end = text.len().min(super::codex_btw::MAX_ANSWER);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}
fn warning(warnings: &mpsc::UnboundedSender<String>, warned: &mut bool) {
    if !*warned {
        let _ = warnings
            .send("side-question transport unavailable; no model turn will be replayed".into());
        *warned = true;
    }
}
async fn run(
    mut backend: Backend,
    generation: String,
    harness: Harness,
    warnings: mpsc::UnboundedSender<String>,
    mut stop: watch::Receiver<bool>,
    mut _hooks: Hooks,
    followups: bool,
) {
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut active: Option<Active> = None;
    let mut held: Option<Held> = None;
    let mut final_report: Option<Final> = None;
    let mut lost: Option<i64> = None;
    let mut attempted = 0i64;
    let mut warned = false;
    let mut ending = false;
    let mut disabled = false;
    loop {
        tokio::select! { biased;
            changed = stop.changed(), if !ending => {
                if changed.is_err() || *stop.borrow() {
                    ending = true;
                    if let Some(a) = &mut active {
                        a.ending = true;
                        let _ = a.cancel.send(true);
                    }
                }
            },
            result = async { (&mut active.as_mut().unwrap().task).await }, if active.is_some() => {
                let a = active.take().unwrap();
                let outcome = result.unwrap_or_else(|_| Outcome {
                    status: Status::Failed, answer: harness.side_text(),
                    detail: "side task stopped".into(), disable: true, retained: false,
                });
                let mut disable = outcome.disable;
                if outcome.retained && !a.ending && !ending && !disabled && !disable {
                    held = Some(Held { conversation: a.conversation, latest: a.id,
                        deadline: Instant::now() + IDLE });
                } else {
                    if followups && outcome.status != Status::Ended { lost = Some(a.id); }
                    held = None;
                    // Early validation/refusal may leave a previously held
                    // child untouched. Dispose it too; never orphan it locally.
                    disable |= !harness.finish_side().await;
                }
                if disable {
                    disabled = true;
                    let _ = availability(&mut backend, &generation, false, followups.then_some(false),
                        "provider side-fork prerequisites or cleanup could not be confirmed; restart after resolving the provider failure").await;
                }
                let status = match outcome.status {
                    Status::Completed => SideStatus::SideCompleted,
                    Status::Ended if followups => SideStatus::SideEnded,
                    _ => SideStatus::SideFailed,
                };
                let answer = if outcome.answer.is_empty() { bounded_detail(outcome.detail) } else { outcome.answer };
                final_report = Some(Final { id: a.id, status,
                    answer: (!answer.is_empty()).then_some(answer) });
            },
            _ = tick.tick() => {},
        }
        // Local expiry and process loss must release the child even when the
        // board cannot be reached. Reporting its loss can wait for the board.
        if held.as_ref().is_some_and(|h| {
            ending || disabled || !harness.side_retained() || Instant::now() >= h.deadline
        }) {
            lost = held.take().map(|h| h.latest);
            if !harness.finish_side().await {
                disabled = true;
                let _ = availability(
                    &mut backend,
                    &generation,
                    false,
                    followups.then_some(false),
                    "side child cleanup could not be confirmed",
                )
                .await;
            }
        }
        // Poll even with a retained but idle child: owner end and provider loss
        // must not wait for another question to release the ephemeral thread.
        let current = match snapshot(&mut backend, &generation).await {
            Ok(r) => {
                warned = false;
                r
            }
            Err(e) => {
                warning(&warnings, &mut warned);
                if super::side::permanent(&backend, &e) {
                    if let Some(a) = &mut active {
                        a.ending = true;
                        let _ = a.cancel.send(true);
                    }
                    ending = true;
                }
                if ending && active.is_none() {
                    break;
                }
                continue;
            }
        };
        if let Some(h) = &held {
            let row = current.side.as_ref();
            if row.is_none_or(|r| r.ended || r.conversation != Some(h.conversation)) {
                if row.is_some_and(|r| r.id == h.latest && !r.ended) {
                    lost = Some(h.latest);
                }
                held = None;
                if !harness.finish_side().await {
                    disabled = true;
                    let _ = availability(
                        &mut backend,
                        &generation,
                        false,
                        followups.then_some(false),
                        "side child cleanup could not be confirmed",
                    )
                    .await;
                }
            }
        }
        if let Some(f) = &final_report {
            let writable = current.side.as_ref().is_some_and(|r| {
                r.id == f.id
                    && if f.status == SideStatus::SideEnded {
                        !r.ended
                    } else {
                        r.status() == SideStatus::SideOpen
                    }
            });
            if !writable || report(&mut backend, &generation, f).await.is_ok() {
                final_report = None;
            } else {
                warning(&warnings, &mut warned);
                if ending {
                    break;
                }
            }
            continue;
        }
        if let Some(id) = lost {
            let writable = current
                .side
                .as_ref()
                .is_some_and(|r| r.id == id && !r.ended);
            if !writable
                || report(
                    &mut backend,
                    &generation,
                    &Final {
                        id,
                        status: SideStatus::SideEnded,
                        answer: None,
                    },
                )
                .await
                .is_ok()
            {
                lost = None;
            } else {
                warning(&warnings, &mut warned);
                if ending {
                    break;
                }
            }
            continue;
        }
        if let Some(a) = active.as_mut() {
            if ending
                || current
                    .side
                    .as_ref()
                    .is_none_or(|r| r.id != a.id || r.ended || r.status() != SideStatus::SideOpen)
            {
                a.ending |= ending
                    || current
                        .side
                        .as_ref()
                        .is_none_or(|r| r.ended || r.conversation != Some(a.conversation));
                let _ = a.cancel.send(true);
            } else {
                if let Some(text) = harness.side_progress()
                    && text != a.reported
                {
                    if report(
                        &mut backend,
                        &generation,
                        &Final {
                            id: a.id,
                            status: SideStatus::SideOpen,
                            answer: Some(text.clone()),
                        },
                    )
                    .await
                    .is_ok()
                    {
                        a.reported = text;
                    } else {
                        warning(&warnings, &mut warned);
                    }
                }
            }
            continue;
        }
        if (ending || disabled)
            && current
                .side
                .as_ref()
                .is_none_or(|r| r.status() != SideStatus::SideOpen || r.id <= attempted)
        {
            break;
        }
        let Some(row) = current
            .side
            .filter(|r| r.status() == SideStatus::SideOpen && r.id > attempted)
        else {
            continue;
        };
        attempted = row.id;
        let claim = timeout(
            CALL,
            backend.claim_side(proto::ClaimSideRequest {
                generation: generation.clone(),
            }),
        )
        .await;
        #[cfg(test)]
        let claim = if let Some((reached, release)) = _hooks.claim_ack.take() {
            let _ = reached.send(());
            if release.await.unwrap_or(true) {
                claim
            } else {
                Ok(Err(tonic::Status::unavailable(
                    "synthetic lost claim acknowledgment",
                )
                .into()))
            }
        } else {
            claim
        };
        let request = match claim {
            Ok(Ok(r)) => r.request,
            _ => {
                disabled = true;
                if let Ok(r) = snapshot(&mut backend, &generation).await
                    && let Some(r) = r.side.filter(|r| r.status() == SideStatus::SideOpen)
                {
                    let _ = report(
                        &mut backend,
                        &generation,
                        &Final {
                            id: r.id,
                            status: SideStatus::SideFailed,
                            answer: r.answer,
                        },
                    )
                    .await;
                }
                let _ = availability(&mut backend, &generation, false, followups.then_some(false),
                    "side claim acknowledgment was uncertain; restart the runner to resume side questions").await;
                warning(&warnings, &mut warned);
                None
            }
        };
        let Some(request) = request else {
            continue;
        };
        attempted = attempted.max(request.id);
        let after_claim = snapshot(&mut backend, &generation).await;
        let still_open = after_claim.as_ref().is_ok_and(|r| {
            r.side.as_ref().is_some_and(|r| {
                r.id == request.id && !r.ended && r.status() == SideStatus::SideOpen
            })
        });
        if !still_open {
            // A definite cancellation before submission leaves a previously
            // quiescent child usable. Advance only the exact claimed row.
            let cancelled = after_claim.as_ref().is_ok_and(|r| {
                r.side.as_ref().is_some_and(|r| {
                    r.id == request.id && !r.ended && r.status() == SideStatus::SideCancelled
                })
            });
            if cancelled
                && let Some(h) = held.as_mut().filter(|h| h.follows(&request))
                && harness.side_retained()
            {
                h.latest = request.id;
                h.deadline = Instant::now() + IDLE;
                continue;
            }
            // A failed verification is not permission to invoke, but the known
            // claim still needs a terminal report if it is open when reachable.
            held = None;
            if !harness.finish_side().await {
                disabled = true;
            }
            final_report = Some(Final {
                id: request.id,
                status: SideStatus::SideFailed,
                answer: Some(
                    "Side question was not submitted; its state could not be confirmed.".into(),
                ),
            });
            if followups {
                lost = Some(request.id);
            }
            continue;
        }
        if ending || disabled {
            let is_followup = followups && request.follows.is_some();
            final_report = Some(Final {
                id: request.id,
                status: if is_followup {
                    SideStatus::SideEnded
                } else {
                    SideStatus::SideFailed
                },
                answer: Some(
                    "Side conversation ended; the runner cannot continue this child.".into(),
                ),
            });
            if followups && !is_followup {
                lost = Some(request.id);
            }
            continue;
        }
        let mode = if request.follows.is_some() {
            if !followups
                || !held.as_ref().is_some_and(|h| h.follows(&request))
                || !harness.side_retained()
            {
                final_report = Some(Final {
                    id: request.id,
                    status: if followups {
                        SideStatus::SideEnded
                    } else {
                        SideStatus::SideFailed
                    },
                    answer: Some("Side conversation ended; start a fresh /btw.".into()),
                });
                continue;
            }
            Mode::Followup
        } else if followups {
            Mode::Fresh
        } else {
            Mode::OneShot
        };
        // A held identity only describes a quiescent child, never an active turn.
        held = None;
        let (cancel, rx) = watch::channel(false);
        let h = harness.clone();
        let task = tokio::spawn(async move { h.side_turn(&request.question, mode, rx).await });
        active = Some(Active {
            id: request.id,
            conversation: request.conversation.unwrap_or(request.id),
            ending: false,
            cancel,
            task,
            reported: String::new(),
        });
    }
    drop(active);
    if !harness.finish_side().await {
        let _ = warnings
            .send("side child cleanup unconfirmed; shared tools may already have run".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::{
        acp::McpServer,
        harness::BbsTools,
        mcp::Tools,
        settings::{HarnessConfig, HarnessType},
    };
    use crate::{config::Config, service::Caller};
    #[tokio::test(start_paused = true)]
    async fn held_identity_and_idle_grace_are_per_conversation_and_per_turn() {
        let mut held = Held {
            conversation: 10,
            latest: 11,
            deadline: Instant::now() + IDLE,
        };
        let mut request = proto::SideRequest {
            conversation: Some(10),
            follows: Some(11),
            ..Default::default()
        };
        assert!(held.follows(&request));
        request.conversation = Some(9);
        assert!(!held.follows(&request));
        request.conversation = Some(10);
        request.follows = Some(10);
        assert!(!held.follows(&request));
        request.follows = Some(11);
        tokio::time::advance(Duration::from_secs(30 * 60)).await;
        assert!(
            held.follows(&request),
            "runner keeps the server's one-minute expiry grace"
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(!held.follows(&request));
        // A completed follow-up refreshes idle time; no conversation-age cap.
        held.deadline = Instant::now() + IDLE;
        held.latest = 12;
        request.follows = Some(12);
        tokio::time::advance(Duration::from_secs(30 * 60)).await;
        assert!(held.follows(&request));
    }
    #[tokio::test]
    async fn claim_ack_uncertainty_cancel_and_legacy_one_shot_routing() {
        for case in 0..3 {
            let lost = case == 0;
            let cancelled = case == 1;
            let dir = tempfile::tempdir().unwrap();
            let config = Config::default();
            let mut db = crate::db::open(&dir.path().join("bbs.db")).unwrap();
            crate::db::migrate(&mut db).unwrap();
            drop(db);
            let mut human = Backend::open(
                &config,
                dir.path(),
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
                    name: Some("ClaimTest".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            let mut board = Backend::open(
                &config,
                dir.path(),
                Caller {
                    token: Some(agent.token),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            board
                .begin_side_session(proto::BeginSideSessionRequest {
                    generation: "claim-generation".into(),
                })
                .await
                .unwrap();
            availability(&mut board, "claim-generation", true, None, "")
                .await
                .unwrap();
            let id = human
                .ask_side(proto::AskSideRequest {
                    follows: None,
                    agent: "ClaimTest".into(),
                    question: "never submit this after uncertainty".into(),
                })
                .await
                .unwrap()
                .id;
            let provider = HarnessConfig {
                kind: HarnessType::CodexAppServer,
                command: vec![
                    "python3".into(),
                    format!("{}/tests/fake_btw.py", env!("CARGO_MANIFEST_DIR")),
                    "normal".into(),
                ],
                ..Default::default()
            };
            let (harness, mut process, _events, _) = Harness::start(
                &provider,
                dir.path(),
                &dir.path().join("stderr"),
                BbsTools {
                    mcp: McpServer {
                        name: "bbs".into(),
                        command: "unused".into(),
                        args: vec![],
                        env: vec![],
                    },
                    tools: Tools::new(dir.path(), "synthetic-unused".into()),
                    input_enabled: true,
                },
                None,
            )
            .await
            .unwrap();
            let (reached, held) = tokio::sync::oneshot::channel();
            let (release, wait) = tokio::sync::oneshot::channel();
            let (stop, rx) = watch::channel(false);
            let (warnings, _) = mpsc::unbounded_channel();
            let task = tokio::spawn(run(
                board,
                "claim-generation".into(),
                harness.clone(),
                warnings,
                rx,
                Hooks {
                    claim_ack: Some((reached, wait)),
                },
                false,
            ));
            timeout(Duration::from_secs(5), held)
                .await
                .unwrap()
                .unwrap();
            if cancelled {
                human
                    .cancel_side(proto::CancelSideRequest { id, end: false })
                    .await
                    .unwrap();
            }
            release.send(!lost).unwrap();
            timeout(Duration::from_secs(5), async {
                loop {
                    let r = human
                        .side_report(proto::SideReportRequest {
                            agent: "ClaimTest".into(),
                            generation: None,
                        })
                        .await
                        .unwrap();
                    if r.side
                        .as_ref()
                        .is_some_and(|s| s.id == id && s.status() != SideStatus::SideOpen)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let _ = stop.send(true);
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            let wire = std::fs::read_to_string(dir.path().join("btw-wire.jsonl")).unwrap();
            let wire: Vec<serde_json::Value> = wire
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            if case == 2 {
                assert_eq!(
                    wire.iter().filter(|v| v["method"] == "thread/fork").count(),
                    1
                );
                assert_eq!(
                    wire.iter()
                        .filter(
                            |v| v["method"] == "turn/start" && v["params"]["threadId"] == "child"
                        )
                        .count(),
                    1
                );
                assert!(wire.iter().any(|v| v["method"] == "thread/unsubscribe"));
                let r = human
                    .side_report(proto::SideReportRequest {
                        agent: "ClaimTest".into(),
                        generation: None,
                    })
                    .await
                    .unwrap();
                assert_eq!(r.side_followups, None);
                assert_eq!(r.side.unwrap().status(), SideStatus::SideCompleted);
            } else {
                assert!(
                    !wire
                        .iter()
                        .any(|v| v["method"] == "thread/fork" || v["method"] == "turn/start")
                );
            }
            let h = harness.clone();
            let parent = tokio::spawn(async move { h.prompt("ordinary parent", None).await });
            timeout(Duration::from_secs(5), async {
                loop {
                    let w = std::fs::read_to_string(dir.path().join("btw-wire.jsonl")).unwrap();
                    if w.contains("parent_active") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            harness.cancel().await.unwrap();
            parent.await.unwrap().unwrap();
            process.stop().await;
        }
    }
}
