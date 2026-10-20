//! Effort UI decisions over driver-advertised values; no provider defaults.

use std::collections::HashMap;

use super::event::{EffortSendError, Request};
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
    revision: u64,
}

#[derive(Default)]
struct Agent {
    report: Option<proto::EffortReport>,
    revision: u64,
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
    awaiting_send: bool,
    after: i64,
}

pub enum Decision {
    Value(String),
    Choose,
    Notice(String),
}

impl Efforts {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn listed(&mut self, agents: &[proto::Agent], requested_at: u64) {
        let absent: Vec<_> = self
            .agents
            .keys()
            .filter(|name| !agents.iter().any(|a| a.name.eq_ignore_ascii_case(name)))
            .cloned()
            .collect();
        for name in absent {
            self.clear(&name, requested_at);
        }
        for agent in agents {
            self.agents
                .entry(agent.name.to_ascii_lowercase())
                .or_default()
                .owner = agent.owner.clone();
            let known = self
                .agents
                .get(&agent.name.to_ascii_lowercase())
                .expect("listed agent");
            if known.revision > requested_at {
                continue;
            }
            if agent.effort.is_none() {
                self.clear(&agent.name, requested_at);
            } else {
                self.report(
                    &agent.name,
                    agent.effort.as_ref(),
                    agent.effort_reported_at.as_deref(),
                );
            }
        }
    }

    fn clear(&mut self, name: &str, requested_at: u64) {
        let Some(agent) = self.agents.get_mut(&name.to_ascii_lowercase()) else {
            return;
        };
        if agent.revision <= requested_at && agent.report.take().is_some() {
            self.revision += 1;
            agent.revision = self.revision;
            // Timestamp fences pre-clear replay; unresolved intent stays separate.
        }
    }

    pub fn summarized(&mut self, agents: &[proto::AgentSummary]) {
        for agent in agents {
            self.report(
                &agent.agent,
                agent.effort.as_ref(),
                agent.effort_reported_at.as_deref(),
            );
        }
    }

    fn report(&mut self, name: &str, report: Option<&proto::EffortReport>, at: Option<&str>) {
        let Some(report) = report else { return };
        let agent = self.agents.entry(name.to_ascii_lowercase()).or_default();
        // Lists and the shared stream can cross in flight. Older server reports
        // cannot replace newer facts or revive a pending control.
        if let (Some(old), Some(new)) = (agent.reported_at.as_deref(), at)
            && let (Ok(old), Ok(new)) = (
                chrono::DateTime::parse_from_rfc3339(old),
                chrono::DateTime::parse_from_rfc3339(new),
            )
            && (new < old || (agent.report.is_none() && new == old))
        {
            return;
        }
        if agent.report.as_ref() == Some(report) && agent.reported_at.as_deref() == at {
            return;
        }
        self.revision += 1;
        agent.revision = self.revision;
        agent.report = Some(report.clone());
        agent.reported_at = at.map(str::to_string);
        agent.reconcile();
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
    pub fn request(&mut self, name: &str, human: &str, value: String, id: u64) -> Option<Request> {
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

    pub fn sent(&mut self, name: &str, id: u64, result: &Result<Option<i64>, EffortSendError>) {
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
                agent.reconcile();
            }
            // A server may have accepted a request even if the transport lost
            // its reply: it stays pending rather than invent failure.
            Ok(None) => {}
            Err(EffortSendError::Rejected(error)) => {
                agent.pending = None;
                agent.reconcile();
                if agent.pending.is_none() {
                    agent.error = Some(format!("Effort request rejected: {error}"));
                }
                return;
            }
            Err(EffortSendError::Unknown(_)) => {}
        }
        agent.reconcile();
    }

    /// Whether `human` owns agent `name`, as the latest agent list says.
    pub fn owned_by(&self, name: &str, human: &str) -> bool {
        self.agents
            .get(&name.to_ascii_lowercase())
            .and_then(|a| a.owner.as_deref())
            .is_some_and(|owner| owner.eq_ignore_ascii_case(human))
    }

    pub fn error(&self, name: &str) -> Option<&str> {
        self.agents
            .get(&name.to_ascii_lowercase())?
            .error
            .as_deref()
    }

    /// The status clipped to `width`, keeping both the current and the
    /// next-turn value visible.
    pub fn summary(&self, name: &str, width: usize) -> String {
        let full = self.status(name);
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
        let fixed = "effort → next".width();
        let part = width.saturating_sub(fixed) / 2;
        let clipped = |value: String| {
            super::theme::clip_line(ratatui::text::Line::raw(value), part).to_string()
        };
        format!(
            "effort {}→{} next",
            clipped(agent.current()),
            clipped(agent.label(&pending.value))
        )
    }

    /// The last reported value and any next-turn change, without report age.
    pub fn status(&self, name: &str) -> String {
        let Some(agent) = self.agents.get(&name.to_ascii_lowercase()) else {
            return "effort unknown".into();
        };
        let state = agent.report.as_ref().and_then(|r| r.state.as_ref());
        let mut text = if state.is_some_and(|s| !s.supported) {
            "effort unsupported".into()
        } else {
            format!("effort {}", agent.current())
        };
        if let Some(pending) = &agent.pending {
            text.push_str(&format!(" → {} next turn", agent.label(&pending.value)));
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
    /// The last reported value's label, or "unknown".
    fn current(&self) -> String {
        self.report
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.current.as_ref())
            .map(|v| self.label(v))
            .unwrap_or_else(|| "unknown".into())
    }

    fn label(&self, value: &str) -> String {
        self.report
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.choices.iter().find(|c| c.value == value))
            .map(label)
            .unwrap_or_else(|| single_line(value))
    }

    fn reconcile(&mut self) {
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

    fn update(e: &mut Efforts, name: &str, r: &proto::EffortReport) {
        e.report(name, Some(r), Some("2026-10-18T19:59:00Z"));
    }

    #[test]
    fn pending_is_separate_uses_next_basis_and_only_exact_outcomes_resolve_it() {
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r);
        assert!(matches!(e.decide("a", "ada", true), Decision::Value(v) if v == "medium"));
        assert!(e.request("A", "ada", "medium".into(), 10).is_some());
        e.sent("A", 10, &Ok(Some(100)));
        assert!(matches!(e.decide("A", "ada", true), Decision::Value(v) if v == "high"));
        // No report age or alarm: the value and the next-turn change only.
        assert_eq!(e.status("A"), "effort low → medium next turn");
        // Current may change in a metadata refresh without answering this request.
        r.state.as_mut().unwrap().current = Some("medium".into());
        r.answered = Some(proto::EffortOutcome {
            request_id: 99,
            result: 1,
            detail: String::new(),
        });
        update(&mut e, "A", &r);
        assert_eq!(e.status("A"), "effort medium → medium next turn");
        r.answered.as_mut().unwrap().request_id = 100;
        update(&mut e, "A", &r);
        assert!(!e.status("A").contains("next turn"));
        assert!(e.error("A").is_none());
    }

    #[test]
    fn newer_reported_pending_supersedes_without_success_and_old_answers_cannot_clear_it() {
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r);
        e.request("A", "ada", "medium".into(), 10);
        e.sent("A", 10, &Ok(Some(100)));
        r.pending = Some(proto::EffortRequest {
            request_id: 101,
            value: "high".into(),
        });
        r.answered = Some(proto::EffortOutcome {
            request_id: 100,
            result: 2,
            detail: "older rejection".into(),
        });
        update(&mut e, "A", &r);
        assert!(e.status("A").contains("effort low"));
        assert!(e.status("A").contains("high next turn"));
        assert!(e.error("A").is_none());
        r.pending = None;
        r.answered = Some(proto::EffortOutcome {
            request_id: 101,
            result: 3,
            detail: "provider disconnected".into(),
        });
        update(&mut e, "A", &r);
        assert!(e.error("A").unwrap().contains("outcome unknown"));
        assert!(!e.status("A").contains("next turn"));
        assert!(e.status("A").contains("effort low"));
    }

    #[test]
    fn reports_before_send_reply_and_crossed_sends_stay_correlated_per_agent() {
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r);
        update(&mut e, "B", &r);
        e.request("A", "ada", "medium".into(), 10);
        e.request("A", "ada", "high".into(), 11);
        e.sent("A", 10, &Err(EffortSendError::Unknown("old error".into())));
        assert!(e.error("A").is_none());
        assert!(!e.status("B").contains("next turn"));
        r.state.as_mut().unwrap().current = Some("high".into());
        r.answered = Some(proto::EffortOutcome {
            request_id: 101,
            result: 1,
            detail: String::new(),
        });
        update(&mut e, "A", &r);
        assert!(e.status("A").contains("next turn"));
        e.sent("A", 11, &Ok(Some(101)));
        assert!(!e.status("A").contains("next turn"));
        // A late list must not roll back the newer report's state.
        e.report(
            "A",
            Some(&report(Some("low"))),
            Some("2026-10-18T19:58:00Z"),
        );
        assert!(e.status("A").contains("effort high"));
    }

    #[test]
    fn unconfirmed_sends_stay_pending_quietly_and_explicit_new_pending_can_replace_them() {
        let mut e = Efforts::default();
        let mut r = report(Some("default"));
        update(&mut e, "A", &r);
        assert!(matches!(e.decide("A", "ada", true), Decision::Choose));
        e.request("A", "ada", "low".into(), 10);
        e.sent("A", 10, &Ok(None));
        assert!(e.error("A").is_none());
        assert!(e.status("A").contains("low next turn"));
        r.pending = Some(proto::EffortRequest {
            request_id: 150,
            value: "high".into(),
        });
        update(&mut e, "A", &r);
        assert!(e.error("A").is_none());
        assert!(e.status("A").contains("high next turn"));
        // Unsupported and changed choices never emit controls.
        r.state.as_mut().unwrap().supported = false;
        r.state.as_mut().unwrap().reason = "provider has no effort setting".into();
        update(&mut e, "A", &r);
        assert!(matches!(e.decide("A", "ada", false), Decision::Notice(_)));
        assert!(e.request("A", "ada", "low".into(), 11).is_none());
        assert!(e.status("A").contains("unsupported"));
        assert!(matches!(
            e.decide("missing", "ada", true),
            Decision::Notice(_)
        ));
    }

    #[test]
    fn rejected_outcome_is_visible_after_reopening_and_owner_checks_do_not_change_state() {
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
            e.revision(),
        );
        assert!(e.error("A").unwrap().contains("clamped"));
        assert!(matches!(e.decide("A", "ada", true), Decision::Notice(_)));
        assert!(e.request("A", "ada", "high".into(), 1).is_none());
        assert!(!e.status("A").contains("next turn"));
    }

    #[test]
    fn authoritative_absence_clears_facts_and_choices_but_keeps_unresolved_intent() {
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r);
        e.request("A", "ada", "medium".into(), 10);
        e.sent("A", 10, &Ok(Some(100)));
        let old_list = e.revision();
        r.state.as_mut().unwrap().current = Some("high".into());
        e.report("A", Some(&r), Some("2026-10-18T20:00:00Z"));
        let empty = proto::Agent {
            name: "A".into(),
            owner: Some("ada".into()),
            ..Default::default()
        };
        e.listed(std::slice::from_ref(&empty), old_list);
        assert!(
            e.status("A").contains("effort high"),
            "crossed old list cannot clear a new report"
        );
        let before_clear = e.revision();
        e.listed(&[empty], before_clear);
        let status = e.status("A");
        assert!(status == "effort unknown → medium next turn", "{status}");
        assert!(e.choices("A").is_empty());
        assert!(matches!(e.decide("A", "ada", true), Decision::Notice(_)));
        // An in-flight old list and replayed old summary cannot restore stale
        // capability/current after retirement or a driver with no report.
        let old = proto::Agent {
            name: "A".into(),
            effort: Some(r.clone()),
            effort_reported_at: Some("2026-10-18T20:00:00Z".into()),
            ..Default::default()
        };
        e.listed(&[old], before_clear);
        e.report("A", Some(&r), Some("2026-10-18T20:00:00Z"));
        assert!(e.choices("A").is_empty());
        e.report("A", Some(&r), Some("2026-10-18T20:01:00Z"));
        assert!(!e.choices("A").is_empty());
        assert!(e.status("A").contains("medium next turn"));
        e.listed(&[], e.revision());
        assert!(
            e.choices("A").is_empty(),
            "retired/absent agents also clear old capability"
        );
        assert!(e.status("A").contains("medium next turn"));
    }

    #[test]
    fn definite_refusal_clears_only_matching_intent_and_unknown_delivery_keeps_it() {
        let mut e = Efforts::default();
        let mut r = report(Some("low"));
        update(&mut e, "A", &r);
        e.request("A", "ada", "medium".into(), 10);
        e.sent(
            "A",
            10,
            &Err(EffortSendError::Rejected("capability unavailable".into())),
        );
        assert!(!e.status("A").contains("next turn"));
        assert!(e.error("A").unwrap().contains("rejected"));
        assert!(
            matches!(e.decide("A", "ada", true), Decision::Value(v) if v == "medium"),
            "refused value is not a stepping basis"
        );
        e.request("A", "ada", "medium".into(), 11);
        e.request("A", "ada", "high".into(), 12);
        e.sent(
            "A",
            11,
            &Err(EffortSendError::Rejected("older A refused".into())),
        );
        assert!(e.status("A").contains("high next turn"));
        assert!(e.error("A").is_none());
        e.sent("A", 12, &Err(EffortSendError::Unknown("deadline".into())));
        assert!(e.status("A").contains("high next turn"));
        assert!(
            e.error("A").is_none(),
            "unknown delivery stays pending quietly"
        );
        // New pending evidence arriving before a refused local send's reply
        // remains pending, without claiming either request succeeded.
        e.request("A", "ada", "medium".into(), 13);
        r.pending = Some(proto::EffortRequest {
            request_id: 200,
            value: "high".into(),
        });
        update(&mut e, "A", &r);
        e.sent(
            "A",
            13,
            &Err(EffortSendError::Rejected("permission".into())),
        );
        assert!(e.status("A").contains("high next turn"));
        assert!(e.status("A").contains("effort low"));
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
