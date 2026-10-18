//! Session configuration, not a measurement of the work a model performed.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::config::Config;
use crate::proto::effort_outcome::Result as Outcome;
use crate::proto::{EffortChoice, EffortOutcome, EffortReport, EffortRequest, EffortState};
use crate::service::Caller;

pub const SET_TIMEOUT: Duration = Duration::from_secs(10);

pub fn unavailable(reason: &str) -> EffortState {
    EffortState {
        reason: reason.into(),
        ..Default::default()
    }
}

pub fn choice(value: &str, label: &str, claude: bool, codex: bool) -> EffortChoice {
    // Provider-documented ranks, never positions in an arbitrary advertised list.
    // https://developers.openai.com/api/docs/guides/reasoning#reasoning-effort
    // https://platform.claude.com/docs/en/build-with-claude/effort#effort-levels
    let levels: &[&str] = if codex {
        &["none", "minimal", "low", "medium", "high", "xhigh", "max"]
    } else if claude {
        &["low", "medium", "high", "xhigh", "max"]
    } else {
        &[]
    };
    EffortChoice {
        value: value.into(),
        label: label.into(),
        rank: levels.iter().position(|v| *v == value).map(|r| r as u32),
    }
}

/// Standard JSON-RPC dispatch/parameter rejection, before method execution.
/// An internal/application/transport error does not prove a setter was unapplied.
pub fn definite_rejection(code: i64) -> bool {
    matches!(code, -32600 | -32601 | -32602)
}

pub fn validate(state: &EffortState, value: &str) -> Result<()> {
    if !state.supported {
        bail!("effort unavailable: {}", state.reason);
    }
    if !state.choices.iter().any(|c| c.value == value) {
        bail!("effort {value:?} is not advertised for this model");
    }
    Ok(())
}

pub fn outcome(id: i64, result: Outcome, detail: impl Into<String>) -> EffortOutcome {
    EffortOutcome {
        request_id: id,
        result: result.into(),
        detail: bounded_detail(&detail.into()),
    }
}

/// Board reports bound diagnostic text; never truncate opaque choice values.
pub fn bounded_detail(text: &str) -> String {
    let mut end = text.len().min(500);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// One driver's control history. A provider update is not a control outcome.
#[derive(Default)]
pub struct Control {
    pub pending: Option<EffortRequest>,
    answered: Option<EffortOutcome>,
    latest: Option<i64>,
    dispatched: Option<i64>,
}

impl Control {
    pub fn request(&mut self, request: EffortRequest, state: &EffortState) {
        self.latest = Some(request.request_id);
        self.pending = None;
        match validate(state, &request.value) {
            Ok(()) => self.pending = Some(request),
            Err(err) => {
                self.answered = Some(outcome(
                    request.request_id,
                    Outcome::Failed,
                    err.to_string(),
                ))
            }
        }
    }

    pub fn dispatch(&mut self) -> Option<EffortRequest> {
        let request = self.pending.as_ref()?;
        if self.dispatched == Some(request.request_id) {
            return None;
        }
        self.dispatched = Some(request.request_id);
        Some(request.clone())
    }

    pub fn interrupted(&mut self) -> bool {
        let Some(request) = self
            .pending
            .as_ref()
            .filter(|r| self.dispatched == Some(r.request_id))
        else {
            return false;
        };
        self.complete(outcome(
            request.request_id,
            Outcome::Unknown,
            "driver stopped before effort confirmation",
        ));
        true
    }

    pub fn complete(&mut self, result: EffortOutcome) {
        // A superseded completion may describe real provider state, but must
        // never clear a newer pending request or overwrite its failure outcome.
        if self.latest != Some(result.request_id) {
            return;
        }
        self.pending = None;
        self.answered = Some(result);
    }

    pub fn report(&self, state: EffortState) -> EffortReport {
        EffortReport {
            state: Some(state),
            pending: self.pending.clone(),
            answered: self.answered.clone(),
        }
    }
}

/// A single ordered writer, independent of activity sharing. Reports are
/// authenticated last-reported state, not proof of a currently connected driver.
/// Existing single-runner ownership is still required; there is no writer lease.
pub fn reporter(
    config: Config,
    dir: PathBuf,
    token: String,
    warn: mpsc::UnboundedSender<String>,
) -> (mpsc::UnboundedSender<EffortReport>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(report_loop(rx, warn, move |report| {
        let config = config.clone();
        let dir = dir.clone();
        let token = token.clone();
        async move {
            let send = async {
                let mut backend = Backend::open(
                    &config,
                    &dir,
                    Caller {
                        token: Some(token),
                        ..Default::default()
                    },
                )
                .await?;
                backend
                    .report_effort(crate::proto::ReportEffortRequest {
                        report: Some(report),
                    })
                    .await?;
                anyhow::Ok(())
            };
            tokio::time::timeout(SET_TIMEOUT, send)
                .await
                .map_err(|_| anyhow::anyhow!("effort report timed out"))?
        }
    }));
    (tx, task)
}

async fn report_loop<F, Fut>(
    mut rx: mpsc::UnboundedReceiver<EffortReport>,
    warn: mpsc::UnboundedSender<String>,
    mut send: F,
) where
    F: FnMut(EffortReport) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut warned = false;
    while let Some(report) = rx.recv().await {
        match send(report).await {
            Ok(()) => warned = false,
            Err(err) => {
                let unsupported = err.chain().any(|e| {
                    e.downcast_ref::<tonic::Status>()
                        .is_some_and(|s| s.code() == tonic::Code::Unimplemented)
                }) || format!("{err:#}")
                    .contains("required capability agent-effort.v1 is unavailable");
                if !warned {
                    let _ = warn.send(format!("effort report unavailable: {err:#}"));
                    warned = true;
                }
                if unsupported {
                    return;
                }
                // No timers or retry loop: only a later natural state change
                // attempts its own snapshot, through this single writer.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn report_outage_recovers_on_next_state_and_warns_once() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (warn, mut warnings) = mpsc::unbounded_channel();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = calls.clone();
        for id in 1..=5 {
            tx.send(EffortReport {
                pending: Some(EffortRequest {
                    request_id: id,
                    value: "high".into(),
                }),
                ..Default::default()
            })
            .unwrap();
        }
        drop(tx);
        report_loop(rx, warn, move |report| {
            let id = report.pending.unwrap().request_id;
            observed.lock().unwrap().push(id);
            std::future::ready(if matches!(id, 1 | 2 | 4) {
                Err(tonic::Status::unavailable("synthetic outage").into())
            } else {
                Ok(())
            })
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), [1, 2, 3, 4, 5]);
        assert!(warnings.try_recv().is_ok());
        assert!(warnings.try_recv().is_ok()); // Success at 3 reset the outage warning.
        assert!(warnings.try_recv().is_err());
    }

    #[tokio::test]
    async fn old_server_disables_reporting_once_without_retry() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (warn, mut warnings) = mpsc::unbounded_channel();
        tx.send(EffortReport::default()).unwrap();
        tx.send(EffortReport::default()).unwrap();
        drop(tx);
        let mut calls = 0;
        report_loop(rx, warn, |_| {
            calls += 1;
            std::future::ready(Err(tonic::Status::unimplemented("old server").into()))
        })
        .await;
        assert_eq!(calls, 1);
        assert!(warnings.try_recv().is_ok());
        assert!(warnings.try_recv().is_err());
    }

    fn state() -> EffortState {
        EffortState {
            supported: true,
            choices: vec![
                choice("low", "Low", false, true),
                choice("high", "High", false, true),
            ],
            current: Some("low".into()),
            reason: String::new(),
        }
    }

    #[test]
    fn stale_outcome_does_not_clear_newer_request_or_failure() {
        let mut c = Control::default();
        c.request(
            EffortRequest {
                request_id: 1,
                value: "high".into(),
            },
            &state(),
        );
        assert_eq!(c.dispatch().unwrap().request_id, 1);
        assert!(c.dispatch().is_none());
        c.request(
            EffortRequest {
                request_id: 2,
                value: "low".into(),
            },
            &state(),
        );
        c.complete(outcome(1, Outcome::Applied, ""));
        assert_eq!(c.pending.as_ref().unwrap().request_id, 2);
        c.request(
            EffortRequest {
                request_id: 3,
                value: "invalid".into(),
            },
            &state(),
        );
        c.complete(outcome(2, Outcome::Applied, ""));
        assert_eq!(c.report(state()).answered.unwrap().request_id, 3);
        assert!(c.pending.is_none());
    }

    #[test]
    fn metadata_is_not_an_ack_and_default_is_not_ranked() {
        let mut c = Control::default();
        c.request(
            EffortRequest {
                request_id: 7,
                value: "high".into(),
            },
            &state(),
        );
        assert!(c.report(state()).answered.is_none());
        for value in ["default", "ultra", "future"] {
            assert_eq!(choice(value, value, true, false).rank, None);
            assert_eq!(choice(value, value, false, true).rank, None);
        }
        assert!(
            choice("xhigh", "Xhigh", true, false).rank < choice("max", "Max", true, false).rank
        );
        assert_eq!(choice("high", "High", false, false).rank, None);
    }
}
