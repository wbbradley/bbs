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

/// Goal prompts per goal before the runner stops it as `limited`.
pub const DEFAULT_BUDGET: u32 = 50;
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
}

/// The wait or block a resumed goal came out of, for its prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    /// Waiting: when the wait was to end. Blocked: none.
    pub until: Option<String>,
    pub reason: String,
    /// True when mail ended it, false when the time came.
    pub by_mail: bool,
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
    pub budget: u32,
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
    Complete { evidence: String },
    Blocked { reason: String },
    Waiting { wake_at: String, reason: String },
}

/// A goal's set time, which is also its identity for updates: to the
/// microsecond, so two goals set within one second differ.
fn now_s() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
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
            reason: String::new(),
            evidence: String::new(),
            wake_at: None,
            idle_turns: 0,
            changed_by: Changer::Owner,
            next: Next::New,
            ended: None,
        }
    }

    fn set(&mut self, status: Status, by: Changer, reason: &str) {
        self.status = status;
        self.changed_by = by;
        self.reason = reason.to_string();
        if status != Status::Waiting {
            self.wake_at = None;
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

    /// The owner resumed it. A goal stopped at its budget gets a fresh one.
    pub fn resume(&mut self) -> Result<()> {
        match self.status {
            Status::Active => bail!("the goal is already active"),
            Status::Complete => bail!("the goal is complete; set a new one instead"),
            Status::Limited => self.iteration = 0,
            _ => {}
        }
        self.set(Status::Active, Changer::Owner, "resumed by owner");
        self.idle_turns = 0;
        self.ended = None;
        self.next = Next::Continue;
        Ok(())
    }

    /// Mail arrived: a waiting or blocked goal resumes. Returns whether it changed.
    pub fn mail_arrived(&mut self) -> bool {
        self.end_wait(true)
    }

    /// The wait's time came. Returns whether it changed.
    pub fn wake(&mut self) -> bool {
        self.status == Status::Waiting && self.end_wait(false)
    }

    fn end_wait(&mut self, by_mail: bool) -> bool {
        if !matches!(self.status, Status::Waiting | Status::Blocked) {
            return false;
        }
        self.ended = Some(Ended {
            until: self.wake_at.clone(),
            reason: self.reason.clone(),
            by_mail,
        });
        let reason = if by_mail {
            "mail arrived"
        } else {
            "the wait ended"
        };
        self.set(Status::Active, Changer::Runner, reason);
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

    /// Apply the agent's update, if it is for this goal while active.
    pub fn apply(&mut self, update: &Update) -> bool {
        if update.goal != self.set_at || self.status != Status::Active {
            return false;
        }
        match &update.kind {
            UpdateKind::Complete { evidence } => {
                self.set(Status::Complete, Changer::Agent, "");
                self.evidence = evidence.clone();
            }
            UpdateKind::Blocked { reason } => self.set(Status::Blocked, Changer::Agent, reason),
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
        if self.iteration >= self.budget {
            let reason = format!("used its budget of {} turns", self.budget);
            self.set(Status::Limited, Changer::Runner, &reason);
        }
        true
    }

    /// Before a goal prompt: an active goal that has used its budget stops as
    /// limited instead. Waits and blocks end turns early, so the check after
    /// each turn alone would miss them. Returns whether it stopped.
    pub fn spent(&mut self) -> bool {
        if self.status != Status::Active || self.iteration < self.budget {
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

    fn render(&self) -> String {
        let budget = format!("budget {}/{} turns", self.iteration, self.budget);
        let header = match self.next {
            Next::New => format!("[bbs goal · new · {budget}]"),
            Next::Resumed => format!("[bbs goal · resumed after wait · {budget}]"),
            Next::Continue | Next::Sharper => {
                format!("[bbs goal · continuation {} · {budget}]", self.iteration)
            }
        };
        let date = self.set_at.get(..10).unwrap_or(&self.set_at);
        let lead = match (self.next, &self.ended) {
            (Next::New, _) => "Start working on it now.".to_string(),
            (Next::Sharper, _) => "Your last turn used no tools and made no progress on the goal. \
                 Take a concrete action now (run a command, edit a file, or call bbs_goal_update)."
                .to_string(),
            (Next::Resumed, Some(ended)) => {
                let was = match &ended.until {
                    Some(until) => format!("You were waiting until {until} for: {}", ended.reason),
                    None => format!("You were blocked: {}", ended.reason),
                };
                let was = if was.ends_with(['.', '?', '!']) {
                    was
                } else {
                    format!("{was}.")
                };
                let why = if ended.by_mail {
                    "New mail arrived."
                } else {
                    "That time has come."
                };
                format!(
                    "{was} {why} Check whether that condition now holds, then continue toward \
                     the objective."
                )
            }
            _ => "Your last turn ended without updating the goal, so the goal is still active."
                .to_string(),
        };
        format!(
            "{header}\nObjective (set by {}, {date}):\n{}\n\n{lead}\n{INSTRUCTIONS}",
            self.set_by,
            self.objective.trim_end(),
        )
    }

    pub fn report(&self) -> proto::GoalReport {
        proto::GoalReport {
            objective: self.objective.clone(),
            status: self.status.proto().into(),
            set_by: self.set_by.clone(),
            set_at: self.set_at.clone(),
            iteration: self.iteration,
            budget: self.budget,
            reason: self.reason.clone(),
            evidence: self.evidence.clone(),
            wake_at: self.wake_at.clone(),
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
- If you can't proceed without a decision or input from someone else: call \
bbs_goal_update(status=\"blocked\", reason=\"...\") and post the question on the board.
- If you need to wait for a time or an outcome: call \
bbs_goal_update(status=\"waiting\", until=\"...\" or for=\"...\", reason=\"...\").
Don't mark the goal complete just to end the loop.";

/// Check the agent's `bbs_goal_update` arguments against `goal`; the update
/// to hand to the runner and the agent's confirmation.
pub fn tool_update(
    goal: Option<&Goal>,
    args: &serde_json::Value,
    max_wait: Duration,
) -> Result<(Update, String)> {
    let Some(goal) = goal.filter(|g| g.status == Status::Active) else {
        bail!("You have no active goal to update.");
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
        "blocked" => (
            UpdateKind::Blocked {
                reason: need("reason", "Blocking the goal")?,
            },
            "Goal blocked. No more goal prompts come until mail arrives; post your question on \
             the board if you have not, then end your turn."
                .to_string(),
        ),
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
                "Waiting until {wake_at}. You will be prompted then, or sooner if mail arrives. \
                 End your turn."
            );
            (UpdateKind::Waiting { wake_at, reason }, reply)
        }
        other => bail!("status must be complete, blocked or waiting, not {other:?}"),
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
    fn prompts_count_and_vary_with_what_happened() {
        let mut g = goal();
        let first = g.prompt();
        assert_eq!(
            first,
            format!(
                "[bbs goal · new · budget 1/3 turns]\nObjective (set by will, 2026-11-05):\n\
                 Ship the release\n\nStart working on it now.\n{INSTRUCTIONS}"
            )
        );
        assert!(g.turn_ended(Some(2)));
        let next = g.prompt();
        assert!(
            next.starts_with("[bbs goal · continuation 2 · budget 2/3 turns]\n"),
            "{next}"
        );
        assert!(next.contains("Your last turn ended without updating the goal"));
        assert!(next.ends_with("Don't mark the goal complete just to end the loop."));
        // An idle turn sharpens the next prompt.
        assert!(g.turn_ended(Some(0)));
        let sharp = g.prompt();
        assert!(sharp.contains("Your last turn used no tools"), "{sharp}");
        // The third prompt used the budget.
        assert!(g.turn_ended(Some(1)));
        assert_eq!(g.status, Status::Limited);
        assert_eq!(g.changed_by, Changer::Runner);
        assert!(!g.spent(), "only an active goal stops");
        // Resuming a limited goal starts a fresh budget.
        g.resume().unwrap();
        assert_eq!((g.status, g.iteration), (Status::Active, 0));
        assert!(
            g.prompt()
                .starts_with("[bbs goal · continuation 1 · budget 1/3 turns]")
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
        // Not active: ignored.
        let done = update(
            &g,
            UpdateKind::Complete {
                evidence: "tagged".into(),
            },
        );
        assert!(!g.apply(&done));
        // The time comes.
        assert!(g.wake());
        assert_eq!((g.status, g.next), (Status::Active, Next::Resumed));
        let text = g.prompt();
        assert!(
            text.starts_with("[bbs goal · resumed after wait · budget 1/3 turns]"),
            "{text}"
        );
        assert!(text.contains(
            "You were waiting until 2026-11-06T09:00:00Z for: CI runs at 0800Z. That time has come."
        ));
        // Blocked, then mail.
        assert!(g.apply(&update(
            &g,
            UpdateKind::Blocked {
                reason: "which branch?".into()
            }
        )));
        assert!(!g.wake(), "a block has no time");
        assert!(g.mail_arrived());
        let text = g.prompt();
        assert!(text.contains("You were blocked: which branch? New mail arrived."));
        assert!(g.apply(&done));
        assert_eq!(
            (g.status, g.evidence.as_str()),
            (Status::Complete, "tagged")
        );
        assert!(g.resume().is_err());
        assert!(!g.mail_arrived());
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
        assert_eq!(g.next, Next::Continue);
    }

    #[test]
    fn tool_updates_are_checked() {
        let g = goal();
        let max = DEFAULT_MAX_WAIT;
        let ok = |args: serde_json::Value| tool_update(Some(&g), &args, max);
        let err = |args: serde_json::Value| format!("{:#}", ok(args).unwrap_err());
        assert!(err(json!({"status": "complete"})).contains("evidence"));
        assert!(err(json!({"status": "complete", "evidence": "  "})).contains("evidence"));
        assert!(err(json!({"status": "blocked"})).contains("reason"));
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
        assert!(err(json!({"status": "done"})).contains("complete, blocked or waiting"));
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
        // No goal, or one that is not active.
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
        let first = update(&g, UpdateKind::Blocked { reason: "a".into() });
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
