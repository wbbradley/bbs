//! Runner-level goals: an objective the runner keeps prompting the agent
//! toward, across turns, until the agent completes it, blocks, waits, or the
//! owner or runner stops it. It works the same for every harness. See
//! HEADLESS.md, "Goals".
//!
//! The agent reports progress with the `bbs_goal_update` tool. The tool may run
//! in another process (the MCP server an ACP harness starts), so it appends
//! each update to `goal-updates.jsonl` in the agent's directory; the runner
//! takes them when the turn ends.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

/// The longest wait an agent may ask for.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// Goal turns in a row with no tool calls before the runner stops the goal.
pub const STALL_TURNS: u32 = 3;
/// How long the restart notice holds goal prompts: the server's drain deadline.
pub const RESTART_HOLD: Duration = Duration::from_secs(20 * 60);
/// Where the agent's tool leaves updates for the runner.
pub const UPDATES_FILE: &str = "goal-updates.jsonl";
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Waiting,
    Blocked,
    Complete,
    Paused,
    Limited,
    Stalled,
    Expired,
}

impl Status {
    fn proto(self) -> proto::GoalStatus {
        match self {
            Status::Active => proto::GoalStatus::Active,
            Status::Waiting => proto::GoalStatus::Waiting,
            Status::Blocked => proto::GoalStatus::Blocked,
            Status::Complete => proto::GoalStatus::Complete,
            Status::Paused => proto::GoalStatus::Paused,
            Status::Limited => proto::GoalStatus::Limited,
            Status::Stalled => proto::GoalStatus::Stalled,
            Status::Expired => proto::GoalStatus::Expired,
        }
    }
}

/// Who last changed a goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Changer {
    Agent,
    Owner,
    Runner,
}

/// Which goal prompt comes next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Next {
    /// The goal was just set.
    #[default]
    New,
    /// The last turn ended with the goal still active.
    Continue,
    /// The last turn used no tools.
    Sharper,
    /// A wait or block ended.
    Resumed,
    /// The owner resumed a stopped goal: restate it in full.
    Restated,
}

/// The wait or block a resumed goal came out of, for its prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    /// Waiting: when the wait was to end. Blocked: none.
    pub until: Option<String>,
    pub reason: String,
    /// True when mail ended it, false when the time came.
    pub by_mail: bool,
    /// Blocked on a blocker: the blocker and how it closed.
    #[serde(default)]
    pub blocker: Option<Closed>,
}

/// A blocker a blocked goal waited on, once it closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub id: i64,
    pub answered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub set_by: String,
    /// RFC 3339.
    pub set_at: String,
    pub status: Status,
    /// Goal prompts sent.
    pub iteration: u32,
    /// The most goal prompts before the goal stops as limited; 0 for none.
    /// The runner's `goal_turn_budget` setting, applied whenever it starts.
    #[serde(default)]
    pub budget: u32,
    /// When the goal expires (RFC 3339); None for no deadline.
    #[serde(default)]
    pub deadline: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub evidence: String,
    /// Waiting: when to wake (RFC 3339).
    #[serde(default)]
    pub wake_at: Option<String>,
    /// Goal turns in a row that used no tools.
    #[serde(default)]
    pub idle_turns: u32,
    pub changed_by: Changer,
    #[serde(default)]
    pub next: Next,
    #[serde(default)]
    pub ended: Option<Ended>,
    /// Blocked: the first version (root) of the agent's blocker it waits on.
    /// None for a goal blocked before blockers were required.
    #[serde(default)]
    pub blocker: Option<i64>,
}

/// An update the agent made with `bbs_goal_update`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    /// The `set_at` of the goal it was made for; others are stale.
    pub goal: String,
    #[serde(flatten)]
    pub kind: UpdateKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateKind {
    Complete {
        evidence: String,
    },
    Blocked {
        reason: String,
        /// The blocker's root; None only in updates from before it was required.
        #[serde(default)]
        blocker: Option<i64>,
    },
    Waiting {
        wake_at: String,
        reason: String,
    },
    /// End a wait or block now.
    Active {},
}

/// The newest set time handed out, in microseconds since the epoch.
static LAST_SET_AT: AtomicI64 = AtomicI64::new(i64::MIN);

/// A goal's set time, which is also its identity for updates: to the
/// microsecond, and strictly later than any this process handed out before,
/// so two goals never share one even when set in the same microsecond.
fn now_s() -> String {
    let now = Utc::now().timestamp_micros();
    let next = |last: i64| now.max(last.saturating_add(1));
    let last = LAST_SET_AT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(next(last)))
        .expect("the update always succeeds");
    DateTime::from_timestamp_micros(next(last))
        .expect("a set time within chrono's range")
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

impl Goal {
    pub fn new(objective: String, set_by: String, budget: u32) -> Goal {
        Goal {
            objective,
            set_by,
            set_at: now_s(),
            status: Status::Active,
            iteration: 0,
            budget,
            deadline: None,
            reason: String::new(),
            evidence: String::new(),
            wake_at: None,
            idle_turns: 0,
            changed_by: Changer::Owner,
            next: Next::New,
            ended: None,
            blocker: None,
        }
    }

    fn set(&mut self, status: Status, by: Changer, reason: &str) {
        self.status = status;
        self.changed_by = by;
        self.reason = reason.to_string();
        if status != Status::Waiting {
            self.wake_at = None;
        }
        if status != Status::Blocked {
            self.blocker = None;
        }
    }

    /// The owner paused it, or cancelled a turn. Returns whether it changed.
    pub fn pause(&mut self, reason: &str) -> bool {
        if !matches!(
            self.status,
            Status::Active | Status::Waiting | Status::Blocked
        ) {
            return false;
        }
        self.set(Status::Paused, Changer::Owner, reason);
        true
    }

    /// The owner resumed it. A goal stopped at its budget gets a fresh one;
    /// an expired goal needs a later deadline first.
    pub fn resume(&mut self) -> Result<()> {
        match self.status {
            Status::Active => bail!("the goal is already active"),
            Status::Complete => bail!("the goal is complete; set a new one instead"),
            Status::Limited => self.iteration = 0,
            Status::Expired if self.past_deadline(Utc::now()) => {
                bail!("the goal's deadline has passed; set a later one, which resumes it")
            }
            _ => {}
        }
        self.set(Status::Active, Changer::Owner, "resumed by owner");
        self.idle_turns = 0;
        self.ended = None;
        self.next = Next::Restated;
        Ok(())
    }

    /// Mail arrived. Mail does not end a wait or a block on a blocker: the
    /// agent reads it and decides. Only a goal blocked by a runner before
    /// blockers were required, with no blocker, resumes. Returns whether it
    /// changed.
    pub fn mail_arrived(&mut self) -> bool {
        self.status == Status::Blocked && self.blocker.is_none() && self.end_wait(true, None)
    }

    /// The wait's time came. Returns whether it changed.
    pub fn wake(&mut self) -> bool {
        self.wake_time().is_some_and(|t| t <= Utc::now()) && self.end_wait(false, None)
    }

    fn past_deadline(&self, now: DateTime<Utc>) -> bool {
        self.deadline
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|d| d <= now)
    }

    /// The deadline passed: a goal still in progress stops as expired.
    /// Returns whether it changed.
    pub fn expire(&mut self) -> bool {
        if !matches!(
            self.status,
            Status::Active | Status::Waiting | Status::Blocked
        ) || !self.past_deadline(Utc::now())
        {
            return false;
        }
        let reason = format!(
            "its deadline {} passed",
            self.deadline.as_deref().unwrap_or_default()
        );
        self.set(Status::Expired, Changer::Runner, &reason);
        true
    }

    /// The owner set the deadline, or cleared it with None. A later deadline
    /// resumes an expired goal. Returns whether the goal resumed.
    pub fn set_deadline(&mut self, deadline: Option<String>) -> bool {
        self.deadline = deadline;
        if self.status != Status::Expired || self.past_deadline(Utc::now()) {
            return false;
        }
        self.set(
            Status::Active,
            Changer::Owner,
            "the owner moved the deadline",
        );
        self.idle_turns = 0;
        self.ended = None;
        self.next = Next::Restated;
        true
    }

    /// When the runner next has to look at the goal: the wait's end or the
    /// deadline, whichever comes first.
    pub fn next_time(&self) -> Option<DateTime<Utc>> {
        let deadline = matches!(
            self.status,
            Status::Active | Status::Waiting | Status::Blocked
        )
        .then(|| self.deadline.as_deref().and_then(parse_time))
        .flatten();
        match (self.wake_time(), deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The blocker a blocked goal waits on, by its root.
    pub fn waits_on(&self) -> Option<i64> {
        self.blocker.filter(|_| self.status == Status::Blocked)
    }

    /// The blocker the goal waits on was answered or cancelled: the goal
    /// resumes. Returns whether it changed.
    pub fn blocker_closed(&mut self, id: i64, answered: bool) -> bool {
        self.waits_on() == Some(id) && self.end_wait(answered, Some(Closed { id, answered }))
    }

    fn end_wait(&mut self, by_mail: bool, blocker: Option<Closed>) -> bool {
        if !matches!(self.status, Status::Waiting | Status::Blocked) {
            return false;
        }
        let reason = match &blocker {
            Some(c) if c.answered => format!("blocker #{} was answered", c.id),
            Some(c) => format!("blocker #{} was cancelled", c.id),
            None if by_mail => "mail arrived".to_string(),
            None => "the wait ended".to_string(),
        };
        self.ended = Some(Ended {
            until: self.wake_at.clone(),
            reason: self.reason.clone(),
            by_mail,
            blocker,
        });
        self.set(Status::Active, Changer::Runner, &reason);
        self.idle_turns = 0;
        self.next = Next::Resumed;
        true
    }

    /// When a waiting goal wakes.
    pub fn wake_time(&self) -> Option<DateTime<Utc>> {
        (self.status == Status::Waiting)
            .then(|| self.wake_at.as_deref().and_then(parse_time))
            .flatten()
    }

    /// Apply the agent's update, if it is for this goal while active,
    /// waiting or blocked.
    pub fn apply(&mut self, update: &Update) -> bool {
        if update.goal != self.set_at
            || !matches!(
                self.status,
                Status::Active | Status::Waiting | Status::Blocked
            )
        {
            return false;
        }
        match &update.kind {
            UpdateKind::Active {} => {
                if self.status == Status::Active {
                    return false;
                }
                self.set(Status::Active, Changer::Agent, "the agent ended the wait");
                self.idle_turns = 0;
                self.ended = None;
                self.next = Next::Continue;
            }
            UpdateKind::Complete { evidence } => {
                self.set(Status::Complete, Changer::Agent, "");
                self.evidence = evidence.clone();
            }
            UpdateKind::Blocked { reason, blocker } => {
                self.set(Status::Blocked, Changer::Agent, reason);
                self.blocker = *blocker;
            }
            UpdateKind::Waiting { wake_at, reason } => {
                self.set(Status::Waiting, Changer::Agent, reason);
                self.wake_at = Some(wake_at.clone());
            }
        }
        true
    }

    /// A turn with this goal's prompt ended. `tools` is the number of tool
    /// calls it made, or None when it failed. Returns whether the goal changed.
    pub fn turn_ended(&mut self, tools: Option<usize>) -> bool {
        if self.status != Status::Active {
            return false;
        }
        if tools == Some(0) {
            self.idle_turns += 1;
            if self.idle_turns >= STALL_TURNS {
                let reason = format!("{STALL_TURNS} turns in a row used no tools");
                self.set(Status::Stalled, Changer::Runner, &reason);
                return true;
            }
            self.next = Next::Sharper;
        } else if tools.is_some() {
            self.idle_turns = 0;
        }
        if self.budget > 0 && self.iteration >= self.budget {
            let reason = format!("used its budget of {} turns", self.budget);
            self.set(Status::Limited, Changer::Runner, &reason);
        }
        true
    }

    /// Before a goal prompt: an active goal that has used its budget stops as
    /// limited instead. Waits and blocks end turns early, so the check after
    /// each turn alone would miss them. Returns whether it stopped.
    pub fn spent(&mut self) -> bool {
        if self.status != Status::Active || self.budget == 0 || self.iteration < self.budget {
            return false;
        }
        let reason = format!("used its budget of {} turns", self.budget);
        self.set(Status::Limited, Changer::Runner, &reason);
        true
    }

    /// The goal block for the next prompt; counts the prompt.
    pub fn prompt(&mut self) -> String {
        self.iteration += 1;
        let text = self.render();
        self.next = Next::Continue;
        self.ended = None;
        text
    }

    /// Undoes `prompt()` for a prompt that was never sent. `before` is the
    /// goal as it was just before `prompt()`. Only the prompt's own changes
    /// are undone, so a pause that arrived meanwhile stands.
    pub fn unprompt(&mut self, before: &Goal) {
        self.iteration = self.iteration.saturating_sub(1);
        self.next = before.next;
        self.ended = before.ended.clone();
    }

    fn render(&self) -> String {
        let tail = self.progress();
        let header = match self.next {
            Next::New => format!("[bbs goal · new · {tail}]"),
            Next::Resumed => format!("[bbs goal · resumed after wait · {tail}]"),
            Next::Restated => format!("[bbs goal · resumed by owner · {tail}]"),
            Next::Continue | Next::Sharper => format!("[bbs goal · continuation · {tail}]"),
        };
        let date = self.set_at.get(..10).unwrap_or(&self.set_at);
        let lead = match (self.next, &self.ended) {
            (Next::New, _) => "Start working on it now.".to_string(),
            (Next::Restated, _) => format!("{} resumed the goal; continue toward it.", self.set_by),
            (Next::Sharper, _) => "Your last turn used no tools and made no progress on the goal. \
                 Take a concrete action now (run a command, edit a file, or call bbs_goal_update)."
                .to_string(),
            (Next::Resumed, Some(ended)) => {
                let was = match (&ended.until, &ended.blocker) {
                    (Some(until), _) => {
                        format!("You were waiting until {until} for: {}", ended.reason)
                    }
                    (None, Some(c)) => {
                        format!("You were blocked on blocker #{}: {}", c.id, ended.reason)
                    }
                    (None, None) => format!("You were blocked: {}", ended.reason),
                };
                let was = if was.ends_with(['.', '?', '!']) {
                    was
                } else {
                    format!("{was}.")
                };
                let why = match (&ended.blocker, ended.by_mail) {
                    (Some(c), _) if c.answered => "It was answered; the answer is in your mail.",
                    (Some(_), _) => "It was cancelled, so it will not be answered.",
                    (None, true) => "New mail arrived.",
                    (None, false) => "That time has come.",
                };
                format!(
                    "{was} {why} Check whether that condition now holds, then continue toward \
                     the objective."
                )
            }
            _ => "Your last turn ended without updating the goal, so the goal is still active."
                .to_string(),
        };
        // The objective and instructions are restated only when the goal is
        // new, the owner resumed it, or the agent looks idle. Otherwise the
        // agent has them in context, and bbs_goal_get rereads them.
        if matches!(self.next, Next::Continue | Next::Resumed) {
            return format!("{header}\n{lead} {REMINDER}");
        }
        format!(
            "{header}\nObjective (set by {}, {date}):\n{}\n\n{lead}\n{INSTRUCTIONS}",
            self.set_by,
            self.objective.trim_end(),
        )
    }

    /// "turn 3", "turn 3 of 50", and " · due <deadline>" when it has one.
    fn progress(&self) -> String {
        let mut text = format!("turn {}", self.iteration);
        if self.budget > 0 {
            text.push_str(&format!(" of {}", self.budget));
        }
        if let Some(deadline) = &self.deadline {
            text.push_str(&format!(" · due {deadline}"));
        }
        text
    }

    /// The goal for `bbs_goal_get`: objective, state and instructions.
    pub fn describe(&self) -> String {
        let status = serde_json::to_value(self.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut state = format!("Status: {status} ({})", self.progress());
        if let Some(until) = self.wake_time().and(self.wake_at.as_deref()) {
            state.push_str(&format!(", waiting until {until}"));
        }
        if let Some(id) = self.waits_on() {
            state.push_str(&format!(", blocked on blocker #{id}"));
        }
        if !self.reason.is_empty() {
            state.push_str(&format!(": {}", self.reason));
        }
        let date = self.set_at.get(..10).unwrap_or(&self.set_at);
        format!(
            "Objective (set by {}, {date}):\n{}\n\n{state}\n{INSTRUCTIONS}",
            self.set_by,
            self.objective.trim_end(),
        )
    }

    /// A line for a prompt that carries mail but no goal block, while the
    /// goal waits or is blocked, so the agent can tell whether the mail ends it.
    pub fn mail_note(&self) -> Option<String> {
        let reason = self.reason.trim_end_matches(['.', '?', '!']);
        match (self.status, &self.wake_at, self.waits_on()) {
            (Status::Waiting, Some(until), _) => Some(format!(
                "(bbs goal: waiting until {until} for: {reason}. If this mail ends the wait, \
                 call bbs_goal_update(status=\"active\"); otherwise handle it and end your turn.)"
            )),
            (Status::Blocked, _, Some(id)) => Some(format!(
                "(bbs goal: blocked on blocker #{id}: {reason}. Goal prompts resume when it is \
                 answered or cancelled. If this mail ends the block, call \
                 bbs_goal_update(status=\"active\").)"
            )),
            _ => None,
        }
    }

    pub fn report(&self) -> proto::GoalReport {
        proto::GoalReport {
            objective: self.objective.clone(),
            status: self.status.proto().into(),
            set_by: self.set_by.clone(),
            set_at: self.set_at.clone(),
            iteration: self.iteration,
            budget: self.budget,
            deadline: self.deadline.clone(),
            reason: self.reason.clone(),
            evidence: self.evidence.clone(),
            wake_at: self.wake_at.clone(),
            blocker_id: self.waits_on(),
            changed_by: match self.changed_by {
                Changer::Agent => "agent",
                Changer::Owner => "owner",
                Changer::Runner => "runner",
            }
            .into(),
        }
    }
}

const INSTRUCTIONS: &str = "\
- If work remains: do the next concrete step now. Don't recap, and don't ask permission for work \
the objective already covers.
- If the objective is fully met: first verify it (run the checks or tests that prove it), then \
call bbs_goal_update(status=\"complete\", evidence=\"...\").
- If you can't proceed without a decision or input from someone else: ask with bbs_blocker_put, \
then call bbs_goal_update(status=\"blocked\", blocker=<its id>, reason=\"...\"). Goal prompts \
resume when that blocker is answered or cancelled.
- If you need to wait for a time or an outcome: call \
bbs_goal_update(status=\"waiting\", until=\"...\" or for=\"...\", reason=\"...\").
- Mail reaches you while you wait or are blocked, without goal prompts. If anything ends the wait \
early, such as mail or a result you were watching for, call bbs_goal_update(status=\"active\").
Don't mark the goal complete just to end the loop.";

/// The line that ends a short goal prompt.
const REMINDER: &str = "bbs_goal_get rereads the objective and instructions; call bbs_goal_update \
when the goal is complete, blocked or waiting.";

/// Check the agent's `bbs_goal_update` arguments against `goal`; the update
/// to hand to the runner and the agent's confirmation.
pub fn tool_update(
    goal: Option<&Goal>,
    args: &serde_json::Value,
    max_wait: Duration,
) -> Result<(Update, String)> {
    let Some(goal) =
        goal.filter(|g| matches!(g.status, Status::Active | Status::Waiting | Status::Blocked))
    else {
        bail!("You have no active, waiting or blocked goal to update.");
    };
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let need = |key: &str, why: &str| text(key).with_context(|| format!("{why} needs {key:?}"));
    let (kind, reply) = match args["status"].as_str().unwrap_or_default() {
        "complete" => (
            UpdateKind::Complete {
                evidence: need("evidence", "Marking the goal complete")?,
            },
            "Goal marked complete. End your turn.".to_string(),
        ),
        "blocked" => {
            let blocker = match &args["blocker"] {
                serde_json::Value::Null => bail!(
                    "Blocking the goal needs \"blocker\": ask with bbs_blocker_put first, then \
                     pass its id."
                ),
                v => v
                    .as_i64()
                    .filter(|id| *id > 0)
                    .with_context(|| format!("blocker {v} is not a blocker id such as 12"))?,
            };
            let reason = text("reason").unwrap_or_else(|| format!("waiting on blocker #{blocker}"));
            (
                UpdateKind::Blocked {
                    reason,
                    blocker: Some(blocker),
                },
                format!(
                    "Goal blocked on blocker #{blocker}. No more goal prompts come until it is \
                     answered or cancelled. End your turn."
                ),
            )
        }
        "waiting" => {
            let reason = need("reason", "Waiting")?;
            let now = Utc::now();
            let until = match (text("until"), text("for")) {
                (Some(until), None) => parse_time(&until).with_context(|| {
                    format!("until {until:?} is not an RFC 3339 time such as 2026-11-06T09:00:00Z")
                })?,
                (None, Some(span)) => {
                    let span = humantime::parse_duration(&span)
                        .with_context(|| format!("for {span:?} is not a duration such as 30m"))?;
                    now + chrono::Duration::from_std(span).context("that wait is too long")?
                }
                _ => bail!("Waiting needs exactly one of \"until\" or \"for\"."),
            };
            if until <= now {
                bail!("{} is not in the future.", rfc3339(until));
            }
            let longest = chrono::Duration::from_std(max_wait).unwrap_or(chrono::Duration::MAX);
            if until - now > longest {
                bail!(
                    "Waits are at most {}; wait for less and wait again if you need to.",
                    humantime::format_duration(max_wait)
                );
            }
            // Whole seconds, rounded up so the wait is never cut short.
            let whole = until
                .with_nanosecond(0)
                .expect("zero is a valid nanosecond");
            let until = if whole < until {
                whole + chrono::Duration::seconds(1)
            } else {
                whole
            };
            let wake_at = rfc3339(until);
            let reply = format!(
                "Waiting until {wake_at}; you will be prompted then. Mail still reaches you \
                 meanwhile. If anything ends the wait early, such as mail or a result you were \
                 watching for, call bbs_goal_update(status=\"active\"). End your turn."
            );
            (UpdateKind::Waiting { wake_at, reason }, reply)
        }
        "active" => {
            if goal.status == Status::Active {
                bail!("The goal is already active.");
            }
            (
                UpdateKind::Active {},
                "Goal active again. Continue toward it; goal prompts resume after this turn."
                    .to_string(),
            )
        }
        other => bail!("status must be complete, blocked, waiting or active, not {other:?}"),
    };
    Ok((
        Update {
            goal: goal.set_at.clone(),
            kind,
        },
        reply,
    ))
}

/// Hand an update to the runner.
pub fn append_update(dir: &Path, update: &Update) -> Result<()> {
    let path = dir.join(UPDATES_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut line = serde_json::to_string(update)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Take the updates handed over so far, oldest first.
pub fn take_updates(dir: &Path) -> Result<Vec<Update>> {
    let path = dir.join(UPDATES_FILE);
    let taking: PathBuf = dir.join(format!("{UPDATES_FILE}.taking"));
    match std::fs::rename(&path, &taking) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("taking {}", path.display())),
    }
    let text = std::fs::read_to_string(&taking)?;
    std::fs::remove_file(&taking)?;
    // A malformed line is skipped: the tool wrote it, so it cannot be fixed here.
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// One ordered writer of goal reports. A report of `None` clears the goal.
pub fn reporter(
    config: Config,
    dir: PathBuf,
    token: String,
    warn: mpsc::UnboundedSender<String>,
) -> (
    mpsc::UnboundedSender<Option<proto::GoalReport>>,
    JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<proto::GoalReport>>();
    let task = tokio::spawn(async move {
        let mut warned = false;
        while let Some(mut goal) = rx.recv().await {
            // Only the newest state matters.
            while let Ok(newer) = rx.try_recv() {
                goal = newer;
            }
            let send = async {
                let mut backend = Backend::open(
                    &config,
                    &dir,
                    Caller {
                        token: Some(token.clone()),
                        ..Default::default()
                    },
                )
                .await?;
                backend
                    .report_goal(proto::ReportGoalRequest { goal })
                    .await?;
                anyhow::Ok(())
            };
            let result = tokio::time::timeout(REPORT_TIMEOUT, send)
                .await
                .map_err(|_| anyhow::anyhow!("goal report timed out"))
                .and_then(|r| r);
            match result {
                Ok(()) => warned = false,
                Err(err) => {
                    let unsupported = format!("{err:#}")
                        .contains("required capability agent-goal.v1 is unavailable");
                    if !warned {
                        let _ = warn.send(format!("goal report unavailable: {err:#}"));
                        warned = true;
                    }
                    if unsupported {
                        return;
                    }
                }
            }
        }
    });
    (tx, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn goal() -> Goal {
        let mut g = Goal::new("Ship the release".into(), "will".into(), 3);
        g.set_at = "2026-11-05T09:00:00Z".into();
        g
    }

    fn update(g: &Goal, kind: UpdateKind) -> Update {
        Update {
            goal: g.set_at.clone(),
            kind,
        }
    }

    #[test]
    fn an_unsent_prompt_is_undone_but_a_pause_stands() {
        let mut g = goal();
        let before = g.clone();
        g.prompt();
        assert!(g.pause("paused by owner"));
        g.unprompt(&before);
        assert_eq!(g.status, Status::Paused);
        assert_eq!(g.iteration, 0);
        assert_eq!(g.next, Next::New);
        assert!(g.resume().is_ok());
        assert!(
            g.prompt()
                .starts_with("[bbs goal · resumed by owner · turn 1 of 3]\n")
        );
    }

    #[test]
    fn prompts_count_and_vary_with_what_happened() {
        let mut g = goal();
        let first = g.prompt();
        assert_eq!(
            first,
            format!(
                "[bbs goal · new · turn 1 of 3]\nObjective (set by will, 2026-11-05):\n\
                 Ship the release\n\nStart working on it now.\n{INSTRUCTIONS}"
            )
        );
        assert!(g.turn_ended(Some(2)));
        let next = g.prompt();
        assert!(
            next.starts_with("[bbs goal · continuation · turn 2 of 3]\n"),
            "{next}"
        );
        // A turn that did work gets a short reminder, without the objective.
        assert_eq!(
            next,
            format!(
                "[bbs goal · continuation · turn 2 of 3]\nYour last turn ended without \
                 updating the goal, so the goal is still active. {REMINDER}"
            )
        );
        // An idle turn restates the goal in full, with a sharper lead.
        assert!(g.turn_ended(Some(0)));
        let sharp = g.prompt();
        assert!(sharp.contains("Your last turn used no tools"), "{sharp}");
        assert!(sharp.contains("Ship the release") && sharp.ends_with(INSTRUCTIONS));
        // The third prompt used the budget.
        assert!(g.turn_ended(Some(1)));
        assert_eq!(g.status, Status::Limited);
        assert_eq!(g.changed_by, Changer::Runner);
        assert!(!g.spent(), "only an active goal stops");
        // Resuming a limited goal starts a fresh budget.
        g.resume().unwrap();
        assert_eq!((g.status, g.iteration), (Status::Active, 0));
        let restated = g.prompt();
        assert!(
            restated
                .starts_with("[bbs goal · resumed by owner · turn 1 of 3]\nObjective (set by will"),
            "{restated}"
        );
    }

    #[test]
    fn waits_do_not_escape_the_budget() {
        let mut g = goal();
        for _ in 0..3 {
            assert!(!g.spent());
            g.prompt();
            let wait = UpdateKind::Waiting {
                wake_at: "2026-11-06T09:00:00Z".into(),
                reason: "r".into(),
            };
            g.apply(&update(&g, wait));
            assert!(!g.turn_ended(Some(1)), "a waiting goal is not accounted");
            assert!(g.wake());
        }
        assert!(g.spent());
        assert_eq!(g.status, Status::Limited);
    }

    #[test]
    fn goals_set_in_one_second_differ() {
        let a = Goal::new("a".into(), "will".into(), 1);
        let b = Goal::new("b".into(), "will".into(), 1);
        assert_ne!(a.set_at, b.set_at);
    }

    #[test]
    fn goals_set_at_once_on_many_threads_all_differ() {
        // Every thread starts together, so many land in one microsecond.
        const THREADS: usize = 16;
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    (0..100).map(|_| now_s()).collect::<Vec<_>>()
                })
            })
            .collect();
        let all: Vec<String> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn idle_turns_stall_the_goal() {
        let mut g = goal();
        g.budget = 50;
        for _ in 0..STALL_TURNS - 1 {
            g.prompt();
            g.turn_ended(Some(0));
            assert_eq!(g.status, Status::Active);
        }
        g.prompt();
        g.turn_ended(Some(0));
        assert_eq!(g.status, Status::Stalled);
        g.resume().unwrap();
        assert_eq!((g.status, g.idle_turns), (Status::Active, 0));
        // Stalled goals keep their count.
        assert_eq!(g.iteration, STALL_TURNS);
    }

    #[test]
    fn agent_updates_apply_only_to_their_active_goal() {
        let mut g = goal();
        let waiting = UpdateKind::Waiting {
            wake_at: "2026-11-06T09:00:00Z".into(),
            reason: "CI runs at 0800Z".into(),
        };
        // Stale: made for another goal.
        let stale = Update {
            goal: "2026-01-01T00:00:00Z".into(),
            kind: waiting.clone(),
        };
        assert!(!g.apply(&stale));
        assert!(g.apply(&update(&g, waiting)));
        assert_eq!(g.status, Status::Waiting);
        assert_eq!(g.wake_time(), parse_time("2026-11-06T09:00:00Z"));
        // Mail does not end the wait; the agent is told, and decides.
        assert!(!g.mail_arrived());
        assert_eq!(
            g.mail_note().as_deref(),
            Some(
                "(bbs goal: waiting until 2026-11-06T09:00:00Z for: CI runs at 0800Z. If this \
                 mail ends the wait, call bbs_goal_update(status=\"active\"); otherwise handle \
                 it and end your turn.)"
            )
        );
        let done = update(
            &g,
            UpdateKind::Complete {
                evidence: "tagged".into(),
            },
        );
        // The time comes.
        assert!(g.wake());
        assert_eq!((g.status, g.next), (Status::Active, Next::Resumed));
        let text = g.prompt();
        assert!(
            text.starts_with("[bbs goal · resumed after wait · turn 1 of 3]"),
            "{text}"
        );
        assert!(text.contains(
            "You were waiting until 2026-11-06T09:00:00Z for: CI runs at 0800Z. That time has come."
        ));
        assert!(
            !text.contains("Ship the release"),
            "a resume is short: {text}"
        );
        assert!(g.mail_note().is_none(), "no note while active");
        // Blocked on a blocker: mail does not end it, but the blocker closing does.
        assert!(g.apply(&update(
            &g,
            UpdateKind::Blocked {
                reason: "which branch?".into(),
                blocker: Some(12),
            }
        )));
        assert_eq!(g.waits_on(), Some(12));
        assert_eq!(g.report().blocker_id, Some(12));
        assert!(
            g.mail_note()
                .unwrap()
                .starts_with("(bbs goal: blocked on blocker #12: which branch. Goal prompts")
        );
        assert!(!g.wake(), "a block has no time");
        assert!(
            !g.mail_arrived(),
            "other mail does not end a block on a blocker"
        );
        assert!(!g.blocker_closed(13, true), "another blocker");
        assert!(g.blocker_closed(12, true));
        assert_eq!(g.reason, "blocker #12 was answered");
        assert_eq!((g.waits_on(), g.blocker), (None, None));
        let text = g.prompt();
        assert!(
            text.contains(
                "You were blocked on blocker #12: which branch? It was answered; the answer is in \
                 your mail."
            ),
            "{text}"
        );
        // A cancelled blocker ends the block too.
        assert!(g.apply(&update(
            &g,
            UpdateKind::Blocked {
                reason: "ship it?".into(),
                blocker: Some(14),
            }
        )));
        assert!(g.blocker_closed(14, false));
        assert!(
            g.prompt()
                .contains("You were blocked on blocker #14: ship it? It was cancelled")
        );
        // A goal blocked by an older runner, with no blocker, still ends on mail.
        assert!(g.apply(&update(
            &g,
            UpdateKind::Blocked {
                reason: "which branch?".into(),
                blocker: None,
            }
        )));
        assert!(g.mail_arrived());
        let text = g.prompt();
        assert!(text.contains("You were blocked: which branch? New mail arrived."));
        // The agent ends a wait itself, from a mail turn; active again is a no-op.
        let wait = UpdateKind::Waiting {
            wake_at: "2026-11-07T09:00:00Z".into(),
            reason: "John's approval".into(),
        };
        assert!(g.apply(&update(&g, wait)));
        assert!(g.apply(&update(&g, UpdateKind::Active {})));
        assert_eq!(
            (g.status, g.changed_by, g.next, g.wake_at.as_deref()),
            (Status::Active, Changer::Agent, Next::Continue, None)
        );
        assert!(!g.apply(&update(&g, UpdateKind::Active {})));
        // Complete straight from a wait.
        let wait = UpdateKind::Waiting {
            wake_at: "2026-11-07T09:00:00Z".into(),
            reason: "CI".into(),
        };
        assert!(g.apply(&update(&g, wait)));
        assert!(g.apply(&done));
        assert_eq!(
            (g.status, g.evidence.as_str()),
            (Status::Complete, "tagged")
        );
        assert!(g.resume().is_err());
        assert!(!g.mail_arrived());
        // Not active, waiting or blocked: ignored.
        assert!(!g.apply(&update(&g, UpdateKind::Active {})));
    }

    #[test]
    fn deadlines_expire_goals_in_progress_and_a_later_one_resumes() {
        let mut g = goal();
        g.budget = 0;
        // No budget: the goal is never limited.
        for _ in 0..5 {
            assert!(!g.spent());
            g.prompt();
            assert!(g.turn_ended(Some(1)));
        }
        assert_eq!((g.status, g.iteration), (Status::Active, 5));
        assert!(
            g.prompt()
                .starts_with("[bbs goal · continuation · turn 6]\n")
        );
        // A future deadline shows in the header and sets the next look.
        assert!(!g.set_deadline(Some("2099-01-01T00:00:00Z".into())));
        assert!(!g.expire());
        assert_eq!(g.next_time(), parse_time("2099-01-01T00:00:00Z"));
        assert!(
            g.describe()
                .contains("Status: active (turn 6 · due 2099-01-01T00:00:00Z)")
        );
        // A wait ends at the earlier of its time and the deadline.
        let wait = UpdateKind::Waiting {
            wake_at: "2098-01-01T00:00:00Z".into(),
            reason: "r".into(),
        };
        assert!(g.apply(&update(&g, wait)));
        assert_eq!(g.next_time(), parse_time("2098-01-01T00:00:00Z"));
        assert!(!g.wake(), "the wait's time has not come");
        g.deadline = Some("2097-01-01T00:00:00Z".into());
        assert_eq!(g.next_time(), parse_time("2097-01-01T00:00:00Z"));
        // The deadline passes while waiting: the goal expires.
        g.deadline = Some("2026-01-01T00:00:00Z".into());
        assert!(g.expire());
        assert_eq!(
            (g.status, g.changed_by, g.reason.as_str()),
            (
                Status::Expired,
                Changer::Runner,
                "its deadline 2026-01-01T00:00:00Z passed"
            )
        );
        assert_eq!(g.report().status(), proto::GoalStatus::Expired);
        assert!(g.next_time().is_none(), "an expired goal has no timer");
        assert!(!g.expire());
        // Resuming needs a later deadline, which resumes it by itself.
        assert!(format!("{:#}", g.resume().unwrap_err()).contains("deadline has passed"));
        assert!(g.set_deadline(Some("2099-01-01T00:00:00Z".into())));
        assert_eq!((g.status, g.next), (Status::Active, Next::Restated));
        assert!(
            g.prompt()
                .starts_with("[bbs goal · resumed by owner · turn 7 · due 2099")
        );
        // Clearing the deadline leaves the goal running.
        assert!(!g.set_deadline(None));
        assert_eq!((g.status, g.next_time()), (Status::Active, None));
    }

    #[test]
    fn owner_pause_and_resume() {
        let mut g = goal();
        assert!(g.pause("cancelled by owner"));
        assert_eq!((g.status, g.changed_by), (Status::Paused, Changer::Owner));
        assert!(!g.pause("again"));
        assert!(!g.mail_arrived(), "mail does not resume a paused goal");
        g.resume().unwrap();
        assert!(g.resume().is_err());
        assert_eq!(g.next, Next::Restated);
    }

    #[test]
    fn tool_updates_are_checked() {
        let g = goal();
        let max = DEFAULT_MAX_WAIT;
        let ok = |args: serde_json::Value| tool_update(Some(&g), &args, max);
        let err = |args: serde_json::Value| format!("{:#}", ok(args).unwrap_err());
        assert!(err(json!({"status": "complete"})).contains("evidence"));
        assert!(err(json!({"status": "complete", "evidence": "  "})).contains("evidence"));
        assert!(err(json!({"status": "blocked", "reason": "r"})).contains("bbs_blocker_put"));
        assert!(err(json!({"status": "blocked", "blocker": "12"})).contains("blocker id"));
        assert!(err(json!({"status": "blocked", "blocker": 0})).contains("blocker id"));
        let (u, reply) = ok(json!({"status": "blocked", "blocker": 12})).unwrap();
        assert_eq!(
            u.kind,
            UpdateKind::Blocked {
                reason: "waiting on blocker #12".into(),
                blocker: Some(12)
            }
        );
        assert!(reply.starts_with("Goal blocked on blocker #12."), "{reply}");
        assert!(err(json!({"status": "waiting", "for": "1h"})).contains("reason"));
        assert!(err(json!({"status": "waiting", "reason": "r"})).contains("exactly one"));
        assert!(
            err(json!({"status": "waiting", "reason": "r", "for": "1h", "until": "2099-01-01T00:00:00Z"}))
                .contains("exactly one")
        );
        assert!(
            err(json!({"status": "waiting", "reason": "r", "until": "2020-01-01T00:00:00Z"}))
                .contains("future")
        );
        assert!(err(json!({"status": "waiting", "reason": "r", "for": "25h"})).contains("at most"));
        assert!(
            err(json!({"status": "waiting", "reason": "r", "for": "soon"})).contains("duration")
        );
        assert!(err(json!({"status": "done"})).contains("complete, blocked, waiting or active"));
        assert!(err(json!({"status": "active"})).contains("already active"));
        let (u, reply) =
            ok(json!({"status": "waiting", "reason": "deploy", "for": "30m"})).unwrap();
        assert!(reply.starts_with("Waiting until "), "{reply}");
        let UpdateKind::Waiting { wake_at, .. } = &u.kind else {
            panic!("{u:?}")
        };
        let wake = parse_time(wake_at).unwrap();
        assert!((wake - Utc::now()).num_minutes() >= 29);
        let (u, _) = ok(json!({"status": "complete", "evidence": "v1 tagged"})).unwrap();
        assert_eq!(u.goal, g.set_at);
        // While waiting, the agent may end the wait or report anything else.
        let mut waiting = goal();
        assert!(waiting.apply(&update(
            &waiting,
            UpdateKind::Waiting {
                wake_at: "2099-01-01T00:00:00Z".into(),
                reason: "r".into(),
            }
        )));
        let (u, reply) = tool_update(Some(&waiting), &json!({"status": "active"}), max).unwrap();
        assert_eq!(u.kind, UpdateKind::Active {});
        assert!(reply.starts_with("Goal active again."), "{reply}");
        assert!(
            tool_update(
                Some(&waiting),
                &json!({"status": "complete", "evidence": "done"}),
                max
            )
            .is_ok()
        );
        // No goal, or one that is not active, waiting or blocked.
        assert!(tool_update(None, &json!({"status": "blocked", "reason": "r"}), max).is_err());
        let mut paused = goal();
        paused.pause("p");
        assert!(
            tool_update(
                Some(&paused),
                &json!({"status": "blocked", "reason": "r"}),
                max
            )
            .is_err()
        );
    }

    #[test]
    fn updates_hand_over_through_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let g = goal();
        assert!(take_updates(dir.path()).unwrap().is_empty());
        let first = update(
            &g,
            UpdateKind::Blocked {
                reason: "a".into(),
                blocker: Some(3),
            },
        );
        let second = update(
            &g,
            UpdateKind::Complete {
                evidence: "b".into(),
            },
        );
        append_update(dir.path(), &first).unwrap();
        append_update(dir.path(), &second).unwrap();
        assert_eq!(take_updates(dir.path()).unwrap(), vec![first, second]);
        assert!(take_updates(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn goals_round_trip_through_state_json() {
        let mut g = goal();
        g.apply(&update(
            &g,
            UpdateKind::Waiting {
                wake_at: "2026-11-06T09:00:00Z".into(),
                reason: "r".into(),
            },
        ));
        let back: Goal = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back, g);
        let report = g.report();
        assert_eq!(report.status(), proto::GoalStatus::Waiting);
        assert_eq!(report.changed_by, "agent");
        assert_eq!(report.wake_at.as_deref(), Some("2026-11-06T09:00:00Z"));
    }
}
