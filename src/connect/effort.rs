//! Effort UI decisions over driver-advertised values; no provider defaults.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::event::Request;
use crate::proto;

#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// The current or pending value has no advertised rank: ask explicitly.
    Choose,
    Value(&'a str),
    /// Already at the requested boundary; do not enqueue a redundant change.
    Boundary,
}

/// Only rank orders choices. Advertised array order and unranked alternatives
/// do not imply a relative effort. Callers use a pending value before current.
pub fn step<'a>(
    choices: &[(&'a str, Option<u32>)],
    basis: Option<&str>,
    increase: bool,
) -> Step<'a> {
    let rank = basis
        .and_then(|basis| choices.iter().find(|(id, _)| *id == basis))
        .and_then(|(_, rank)| *rank);
    let Some(rank) = rank else {
        return Step::Choose;
    };
    choices
        .iter()
        .filter_map(|(id, candidate)| {
            let candidate = (*candidate)?;
            ((increase && candidate > rank) || (!increase && candidate < rank))
                .then_some((*id, candidate))
        })
        .min_by(|(a_id, a), (b_id, b)| {
            let order = if increase { a.cmp(b) } else { b.cmp(a) };
            order.then_with(|| a_id.cmp(b_id))
        })
        .map_or(Step::Boundary, |(id, _)| Step::Value(id))
}

/// Session state survives closing/reopening a conversation. Reports are facts;
/// queued requests remain separate until their exact outcome is reported.
#[derive(Default)]
pub struct Efforts {
    agents: HashMap<String, Agent>,
}

#[derive(Default)]
struct Agent {
    report: Option<proto::EffortReport>,
    reported_at: Option<String>,
    owner: Option<String>,
    pending: Option<Pending>,
    error: Option<String>,
    /// Do not resurrect an already answered/superseded request from a stale report.
    resolved: i64,
}

struct Pending {
    local_id: Option<u64>,
    request_id: Option<i64>,
    value: String,
    since: Instant,
    awaiting_send: bool,
    after: i64,
}

pub enum Decision {
    Value(String),
    Choose,
    Notice(String),
}

impl Efforts {
    pub fn listed(&mut self, agents: &[proto::Agent], now: Instant) {
        for agent in agents {
            self.agents
                .entry(agent.name.to_ascii_lowercase())
                .or_default()
                .owner = agent.owner.clone();
            self.report(
                &agent.name,
                agent.effort.as_ref(),
                agent.effort_reported_at.as_deref(),
                now,
            );
        }
    }

    pub fn summarized(&mut self, agents: &[proto::AgentSummary], now: Instant) {
        for agent in agents {
            self.report(
                &agent.agent,
                agent.effort.as_ref(),
                agent.effort_reported_at.as_deref(),
                now,
            );
        }
    }

    fn report(
        &mut self,
        name: &str,
        report: Option<&proto::EffortReport>,
        at: Option<&str>,
        now: Instant,
    ) {
        let Some(report) = report else { return };
        let agent = self.agents.entry(name.to_ascii_lowercase()).or_default();
        // Lists and the shared stream can cross in flight. Older server reports
        // cannot replace newer facts or revive a pending control.
        if let (Some(old), Some(new)) = (agent.reported_at.as_deref(), at)
            && let (Ok(old), Ok(new)) = (
                chrono::DateTime::parse_from_rfc3339(old),
                chrono::DateTime::parse_from_rfc3339(new),
            )
            && new < old
        {
            return;
        }
        agent.report = Some(report.clone());
        agent.reported_at = at.map(str::to_string);
        agent.reconcile(now);
    }

    pub fn choices(&self, name: &str) -> Vec<(String, String)> {
        self.agents
            .get(&name.to_ascii_lowercase())
            .and_then(|a| a.report.as_ref())
            .and_then(|r| r.state.as_ref())
            .filter(|s| s.supported)
            .map(|s| {
                s.choices
                    .iter()
                    .map(|c| (c.value.clone(), label(c)))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn decide(&self, name: &str, human: &str, increase: bool) -> Decision {
        let Some(agent) = self.agents.get(&name.to_ascii_lowercase()) else {
            return Decision::Notice("Effort unavailable: no driver report yet.".into());
        };
        if agent
            .owner
            .as_deref()
            .is_some_and(|o| !o.eq_ignore_ascii_case(human))
        {
            return Decision::Notice("Only this agent's owner can change effort.".into());
        }
        let Some(state) = agent.report.as_ref().and_then(|r| r.state.as_ref()) else {
            return Decision::Notice("Effort unavailable: no driver report yet.".into());
        };
        if !state.supported || state.choices.is_empty() {
            return Decision::Notice(single_line(&format!(
                "Effort unavailable: {}",
                if state.reason.is_empty() {
                    "driver advertises no choices"
                } else {
                    &state.reason
                }
            )));
        }
        let choices: Vec<_> = state
            .choices
            .iter()
            .map(|c| (c.value.as_str(), c.rank))
            .collect();
        let basis = agent
            .pending
            .as_ref()
            .map(|p| p.value.as_str())
            .or(state.current.as_deref());
        match step(&choices, basis, increase) {
            Step::Choose => Decision::Choose,
            Step::Value(value) => Decision::Value(value.to_string()),
            Step::Boundary => {
                Decision::Notice("Effort is already at that advertised boundary.".into())
            }
        }
    }

    /// Revalidate the explicit value against the latest report at submission.
    pub fn request(
        &mut self,
        name: &str,
        human: &str,
        value: String,
        id: u64,
        now: Instant,
    ) -> Option<Request> {
        let agent = self.agents.get_mut(&name.to_ascii_lowercase())?;
        if agent
            .owner
            .as_deref()
            .is_some_and(|o| !o.eq_ignore_ascii_case(human))
        {
            agent.error = Some("Only this agent's owner can change effort.".into());
            return None;
        }
        let valid = agent
            .report
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .is_some_and(|s| s.supported && s.choices.iter().any(|c| c.value == value));
        if !valid {
            agent.error = Some("Effort choice is no longer available.".into());
            return None;
        }
        agent.pending = Some(Pending {
            local_id: Some(id),
            request_id: None,
            value: value.clone(),
            since: now,
            awaiting_send: true,
            after: agent.resolved.max(
                agent
                    .report
                    .as_ref()
                    .and_then(|r| r.pending.as_ref())
                    .map_or(0, |p| p.request_id),
            ),
        });
        agent.error = None;
        Some(Request::SetEffort {
            id,
            agent: name.to_string(),
            value,
        })
    }

    pub fn sent(
        &mut self,
        name: &str,
        id: u64,
        result: &Result<Option<i64>, String>,
        now: Instant,
    ) {
        let Some(agent) = self.agents.get_mut(&name.to_ascii_lowercase()) else {
            return;
        };
        let Some(pending) = &mut agent.pending else {
            return;
        };
        if pending.local_id != Some(id) {
            return;
        }
        pending.awaiting_send = false;
        match result {
            Ok(Some(request_id)) => {
                pending.request_id = Some(*request_id);
                agent.reconcile(now);
            }
            // A server may have accepted a request even if the transport lost
            // its reply. Keep it visibly unconfirmed rather than invent failure.
            Ok(None) => {
                agent.error =
                    Some("Effort request unconfirmed: server returned no request ID.".into())
            }
            Err(error) => agent.error = Some(format!("Effort request unconfirmed: {error}")),
        }
        agent.reconcile(now);
    }

    pub fn error(&self, name: &str) -> Option<&str> {
        self.agents
            .get(&name.to_ascii_lowercase())?
            .error
            .as_deref()
    }

    /// Bounded summary prioritizes both last-reported and next-turn values.
    pub fn summary(
        &self,
        name: &str,
        now: Instant,
        wall: chrono::DateTime<chrono::Local>,
        width: usize,
    ) -> String {
        let full = self.status(name, now, wall);
        use unicode_width::UnicodeWidthStr;
        if full.width() <= width {
            return full;
        }
        let Some(agent) = self.agents.get(&name.to_ascii_lowercase()) else {
            return full;
        };
        let Some(pending) = &agent.pending else {
            return full;
        };
        let current = agent
            .report
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.current.as_ref())
            .map(|v| agent.label(v))
            .unwrap_or_else(|| "unknown".into());
        let age = agent
            .reported_at
            .as_deref()
            .map(|at| crate::ui::message::age(at, wall))
            .unwrap_or_else(|| "not reported".into());
        let fixed = format!("effort → next ({age})").width();
        let part = width.saturating_sub(fixed) / 2;
        let clipped = |value: String| {
            super::theme::clip_line(ratatui::text::Line::raw(value), part).to_string()
        };
        format!(
            "effort {}→{} next ({age})",
            clipped(current),
            clipped(agent.label(&pending.value))
        )
    }

    pub fn delayed(&self, name: &str, now: Instant) -> Option<String> {
        let agent = self.agents.get(&name.to_ascii_lowercase())?;
        let pending = agent.pending.as_ref()?;
        (now.saturating_duration_since(pending.since) >= Duration::from_secs(60)).then(|| {
            format!(
                "Effort unconfirmed: {} (waiting for next-turn outcome).",
                agent.label(&pending.value)
            )
        })
    }

    pub fn status(
        &self,
        name: &str,
        now: Instant,
        wall: chrono::DateTime<chrono::Local>,
    ) -> String {
        let Some(agent) = self.agents.get(&name.to_ascii_lowercase()) else {
            return "effort unknown (not reported)".into();
        };
        let state = agent.report.as_ref().and_then(|r| r.state.as_ref());
        let current = state
            .and_then(|s| s.current.as_ref())
            .map(|v| agent.label(v))
            .unwrap_or_else(|| "unknown".into());
        let age = agent
            .reported_at
            .as_deref()
            .map(|at| crate::ui::message::age(at, wall));
        let mut text = if state.is_some_and(|s| !s.supported) {
            "effort unsupported".into()
        } else {
            format!("effort {current}")
        };
        text.push_str(&age.map_or_else(
            || " (not reported)".into(),
            |age| format!(" (reported {age})"),
        ));
        if let Some(pending) = &agent.pending {
            text.push_str(&format!(
                " → {} next turn{}",
                agent.label(&pending.value),
                if now.saturating_duration_since(pending.since) >= Duration::from_secs(60) {
                    " (unconfirmed)"
                } else {
                    ""
                }
            ));
        }
        text
    }
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn label(choice: &proto::EffortChoice) -> String {
    single_line(if choice.label.is_empty() {
        &choice.value
    } else {
        &choice.label
    })
}

impl Agent {
    fn label(&self, value: &str) -> String {
        self.report
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.choices.iter().find(|c| c.value == value))
            .map(label)
            .unwrap_or_else(|| single_line(value))
    }

    fn reconcile(&mut self, now: Instant) {
        let Some(report) = &self.report else { return };
        let current = self.pending.as_ref().and_then(|p| p.request_id);
        if let Some(outcome) = &report.answered {
            let fresh = outcome.request_id > self.resolved;
            self.resolved = self.resolved.max(outcome.request_id);
            if current == Some(outcome.request_id) || (self.pending.is_none() && fresh) {
                self.pending = None;
                self.error = match outcome.result() {
                    proto::effort_outcome::Result::Applied => None,
                    proto::effort_outcome::Result::Failed => {
                        Some(format!("Effort failed: {}", outcome.detail))
                    }
                    _ => Some(format!("Effort outcome unknown: {}", outcome.detail)),
                };
            }
        }
        if let Some(pending) = &report.pending
            && pending.request_id > self.resolved
        {
            let replace = match &self.pending {
                None => true,
                Some(local) => local.request_id.map_or(
                    !local.awaiting_send && pending.request_id > local.after,
                    |id| pending.request_id > id,
                ),
            };
            if replace {
                // Explicit newer pending supersedes older local intent. This
                // says nothing about whether either request was applied.
                self.pending = Some(Pending {
                    local_id: None,
                    request_id: Some(pending.request_id),
                    value: pending.value.clone(),
                    since: now,
                    awaiting_send: false,
                    after: pending.request_id,
                });
                self.error = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(current: Option<&str>) -> proto::EffortReport {
        proto::EffortReport {
            state: Some(proto::EffortState {
                supported: true,
                reason: String::new(),
                current: current.map(str::to_string),
                choices: [
                    ("high", Some(9)),
                    ("default", None),
                    ("low", Some(1)),
                    ("medium", Some(4)),
                ]
                .into_iter()
                .map(|(value, rank)| proto::EffortChoice {
                    value: value.into(),
                    label: value.into(),
                    rank,
                })
                .collect(),
            }),
            pending: None,
            answered: None,
        }
    }

    fn wall() -> chrono::DateTime<chrono::Local> {
        chrono::DateTime::parse_from_rfc3339("2026-10-18T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Local)
    }

    fn update(e: &mut Efforts, name: &str, r: &proto::EffortReport, now: Instant) {
        e.report(name, Some(r), Some("2026-10-18T19:59:00Z"), now);
    }

    #[test]
    fn pending_is_separate_uses_next_basis_and_only_exact_outcomes_resolve_it() {
        let now = Instant::now();
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r, now);
        assert!(matches!(e.decide("a", "ada", true), Decision::Value(v) if v == "medium"));
        assert!(e.request("A", "ada", "medium".into(), 10, now).is_some());
        e.sent("A", 10, &Ok(Some(100)), now);
        assert!(matches!(e.decide("A", "ada", true), Decision::Value(v) if v == "high"));
        let status = e.status("A", now, wall());
        assert!(
            status.contains("effort low")
                && status.contains("reported 1m ago")
                && status.contains("medium next turn"),
            "{status}"
        );
        // Current may change in a metadata refresh without answering this request.
        r.state.as_mut().unwrap().current = Some("medium".into());
        r.answered = Some(proto::EffortOutcome {
            request_id: 99,
            result: 1,
            detail: String::new(),
        });
        update(&mut e, "A", &r, now);
        assert!(
            e.status("A", now + Duration::from_secs(61), wall())
                .contains("next turn (unconfirmed)")
        );
        r.answered.as_mut().unwrap().request_id = 100;
        update(&mut e, "A", &r, now);
        assert!(!e.status("A", now, wall()).contains("next turn"));
        assert!(e.error("A").is_none());
    }

    #[test]
    fn newer_reported_pending_supersedes_without_success_and_old_answers_cannot_clear_it() {
        let now = Instant::now();
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r, now);
        e.request("A", "ada", "medium".into(), 10, now);
        e.sent("A", 10, &Ok(Some(100)), now);
        r.pending = Some(proto::EffortRequest {
            request_id: 101,
            value: "high".into(),
        });
        r.answered = Some(proto::EffortOutcome {
            request_id: 100,
            result: 2,
            detail: "older rejection".into(),
        });
        update(&mut e, "A", &r, now);
        assert!(e.status("A", now, wall()).contains("effort low"));
        assert!(e.status("A", now, wall()).contains("high next turn"));
        assert!(e.error("A").is_none());
        update(&mut e, "A", &r, now + Duration::from_secs(61));
        assert!(
            e.status("A", now + Duration::from_secs(61), wall())
                .contains("unconfirmed"),
            "metadata does not refresh pending age"
        );
        r.pending = None;
        r.answered = Some(proto::EffortOutcome {
            request_id: 101,
            result: 3,
            detail: "provider disconnected".into(),
        });
        update(&mut e, "A", &r, now);
        assert!(e.error("A").unwrap().contains("outcome unknown"));
        assert!(!e.status("A", now, wall()).contains("next turn"));
        assert!(e.status("A", now, wall()).contains("effort low"));
    }

    #[test]
    fn reports_before_send_reply_and_crossed_sends_stay_correlated_per_agent() {
        let now = Instant::now();
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r, now);
        update(&mut e, "B", &r, now);
        e.request("A", "ada", "medium".into(), 10, now);
        e.request("A", "ada", "high".into(), 11, now);
        e.sent("A", 10, &Err("old error".into()), now);
        assert!(e.error("A").is_none());
        assert!(!e.status("B", now, wall()).contains("next turn"));
        r.state.as_mut().unwrap().current = Some("high".into());
        r.answered = Some(proto::EffortOutcome {
            request_id: 101,
            result: 1,
            detail: String::new(),
        });
        update(&mut e, "A", &r, now);
        assert!(e.status("A", now, wall()).contains("next turn"));
        e.sent("A", 11, &Ok(Some(101)), now);
        assert!(!e.status("A", now, wall()).contains("next turn"));
        // A late list must not roll back the newer report's state.
        e.report(
            "A",
            Some(&report(Some("low"))),
            Some("2026-10-18T19:58:00Z"),
            now,
        );
        assert!(e.status("A", now, wall()).contains("effort high"));
    }

    #[test]
    fn unconfirmed_sends_remain_visible_and_explicit_new_pending_can_replace_them() {
        let now = Instant::now();
        let mut e = Efforts::default();
        let mut r = report(Some("default"));
        update(&mut e, "A", &r, now);
        assert!(matches!(e.decide("A", "ada", true), Decision::Choose));
        e.request("A", "ada", "low".into(), 10, now);
        e.sent("A", 10, &Ok(None), now);
        assert!(e.error("A").unwrap().contains("unconfirmed"));
        assert!(e.status("A", now, wall()).contains("low next turn"));
        r.pending = Some(proto::EffortRequest {
            request_id: 150,
            value: "high".into(),
        });
        update(&mut e, "A", &r, now);
        assert!(e.error("A").is_none());
        assert!(e.status("A", now, wall()).contains("high next turn"));
        // Unsupported and changed choices never emit controls.
        r.state.as_mut().unwrap().supported = false;
        r.state.as_mut().unwrap().reason = "provider has no effort setting".into();
        update(&mut e, "A", &r, now);
        assert!(matches!(e.decide("A", "ada", false), Decision::Notice(_)));
        assert!(e.request("A", "ada", "low".into(), 11, now).is_none());
        assert!(e.status("A", now, wall()).contains("unsupported"));
        assert!(matches!(
            e.decide("missing", "ada", true),
            Decision::Notice(_)
        ));
    }

    #[test]
    fn rejected_outcome_is_visible_after_reopening_and_owner_checks_do_not_change_state() {
        let now = Instant::now();
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        r.answered = Some(proto::EffortOutcome {
            request_id: 12,
            result: 2,
            detail: "clamped".into(),
        });
        e.listed(
            &[proto::Agent {
                name: "A".into(),
                owner: Some("other".into()),
                effort: Some(r),
                ..Default::default()
            }],
            now,
        );
        assert!(e.error("A").unwrap().contains("clamped"));
        assert!(matches!(e.decide("A", "ada", true), Decision::Notice(_)));
        assert!(e.request("A", "ada", "high".into(), 1, now).is_none());
        assert!(!e.status("A", now, wall()).contains("next turn"));
    }

    #[test]
    fn steps_use_ranks_skip_unranked_options_and_clamp() {
        let choices = [
            ("high", Some(9)),
            ("default", None),
            ("low", Some(1)),
            ("unrecognized", None),
            ("medium", Some(4)),
        ];
        assert_eq!(step(&choices, Some("low"), true), Step::Value("medium"));
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high"));
        assert_eq!(step(&choices, Some("high"), false), Step::Value("medium"));
        assert_eq!(step(&choices, Some("medium"), false), Step::Value("low"));
        assert_eq!(step(&choices, Some("low"), false), Step::Boundary);
        assert_eq!(step(&choices, Some("high"), true), Step::Boundary);
    }

    #[test]
    fn unknown_default_and_missing_values_never_get_an_invented_rank() {
        let choices = [("high", Some(9)), ("default", None), ("custom", None)];
        for basis in [
            None,
            Some("default"),
            Some("custom"),
            Some("not-advertised"),
        ] {
            for increase in [false, true] {
                assert_eq!(step(&choices, basis, increase), Step::Choose);
            }
        }
        assert_eq!(step(&[], Some("low"), true), Step::Choose);
        assert_eq!(
            step(&[("minimal", Some(0))], Some("minimal"), false),
            Step::Boundary
        );
    }

    #[test]
    fn rank_ties_have_a_stable_choice_independent_of_report_array_order() {
        let mut choices = [
            ("medium", Some(1)),
            ("high-b", Some(2)),
            ("high-a", Some(2)),
        ];
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high-a"));
        choices.reverse();
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high-a"));
    }
}
