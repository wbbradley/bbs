//! Board lifecycle for one side request, independent of parent mail and metadata.
use super::{
    codex_btw::{Outcome, Status},
    harness::Harness,
};
use crate::{
    backend::Backend,
    proto::{self, SideStatus},
};
use anyhow::Result;
use std::time::Duration;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, timeout},
};

const CALL: Duration = Duration::from_secs(3);
pub struct Btw {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
struct Active {
    id: i64,
    cancel: watch::Sender<bool>,
    task: JoinHandle<Outcome>,
    reported: String,
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
        let supported = harness.side_supported();
        if availability(
            &mut backend,
            &generation,
            supported,
            if supported {
                "Codex ephemeral fork and child goal-clear are required; shared tools are allowed"
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
        let task = tokio::spawn(run(backend, generation, harness, warnings, rx));
        Some(Self { stop, task })
    }
    pub async fn finish(mut self) {
        let _ = self.stop.send(true);
        if timeout(Duration::from_secs(15), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}
async fn availability(b: &mut Backend, g: &str, supported: bool, reason: &str) -> Result<()> {
    timeout(
        CALL,
        b.report_context(proto::ReportContextRequest {
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
    Ok(timeout(
        CALL,
        b.side_report(proto::SideReportRequest {
            agent: String::new(),
            generation: Some(g.into()),
        }),
    )
    .await??)
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
) {
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut active: Option<Active> = None;
    let mut final_report: Option<Final> = None;
    let mut attempted = 0i64;
    let mut warned = false;
    let mut ending = false;
    let mut disabled = false;
    loop {
        if (ending || disabled) && active.is_none() && final_report.is_none() {
            break;
        }
        tokio::select! {biased;
            changed=stop.changed(),if !ending=>{if changed.is_err()||*stop.borrow(){ending=true;if let Some(a)=&active{let _=a.cancel.send(true);}}},
            result=async{(&mut active.as_mut().unwrap().task).await},if active.is_some()=>{
                let a=active.take().unwrap();
                let outcome=result.unwrap_or_else(|_|Outcome{status:Status::Failed,answer:harness.side_text(),detail:"side task stopped".into(),disable:true});
                if outcome.disable{
                    disabled=true;
                    let _=availability(&mut backend,&generation,false,"provider side-fork prerequisites or cleanup could not be confirmed; restart after resolving the provider failure").await;
                }
                let status=if outcome.status==Status::Completed{SideStatus::SideCompleted}else{SideStatus::SideFailed};
                // Diagnostics stay out of the parent activity/owner outbox. The
                // dedicated pane shows the terminal status and any exact prefix.
                let _detail=outcome.detail;
                final_report=Some(Final{id:a.id,status,answer:(!outcome.answer.is_empty()).then_some(outcome.answer)});
            },
            _=tick.tick()=>{},
        }
        let current = match snapshot(&mut backend, &generation).await {
            Ok(r) => {
                warned = false;
                r
            }
            Err(e) => {
                warning(&warnings, &mut warned);
                if super::side::permanent(&backend, &e) {
                    if let Some(a) = &active {
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
        if let Some(f) = &final_report {
            let open = current
                .side
                .as_ref()
                .is_some_and(|r| r.id == f.id && r.status() == SideStatus::SideOpen);
            if !open {
                final_report = None;
            } else if report(&mut backend, &generation, f).await.is_ok() {
                final_report = None;
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
                    .is_none_or(|r| r.id != a.id || r.status() != SideStatus::SideOpen)
            {
                let _ = a.cancel.send(true);
            } else {
                let text = harness.side_text();
                if text != a.reported {
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
        if ending || disabled {
            continue;
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
        let request = match claim {
            Ok(Ok(r)) => r.request,
            _ => {
                // Even an idempotent board claim is not permission to invoke a
                // model after an ambiguous ACK. Retire the exact claim, if ours.
                disabled = true;
                if let Ok(r) = snapshot(&mut backend, &generation).await {
                    if let Some(r) = r.side.filter(|r| r.status() == SideStatus::SideOpen) {
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
                }
                let _=availability(&mut backend,&generation,false,"side claim acknowledgment was uncertain; restart the runner to resume side questions").await;
                warning(&warnings, &mut warned);
                None
            }
        };
        let Some(request) = request else {
            continue;
        };
        attempted = attempted.max(request.id);
        // Owner cancellation may have committed while the claim reply was in flight.
        let still_open = snapshot(&mut backend, &generation).await.is_ok_and(|r| {
            r.side
                .is_some_and(|r| r.id == request.id && r.status() == SideStatus::SideOpen)
        });
        if !still_open {
            final_report = Some(Final {
                id: request.id,
                status: SideStatus::SideFailed,
                answer: request.answer,
            });
            continue;
        }
        let (cancel, rx) = watch::channel(false);
        let h = harness.clone();
        let question = request.question;
        let task = tokio::spawn(async move { h.side_question(&question, rx).await });
        active = Some(Active {
            id: request.id,
            cancel,
            task,
            reported: String::new(),
        });
    }
    if let Some(a) = active {
        let _ = a.cancel.send(true);
    }
}
