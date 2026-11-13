//! `bbs headless`: run a coding agent without a terminal, driven by its BBS
//! mail. See HEADLESS.md.

mod acp;
mod codex;
mod context;
mod effort;
mod goal;
mod harness;
mod input;
mod log;
mod mcp;
mod outbox;
mod rpc;
mod run;
mod settings;
mod share;
mod side;
mod state;

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::backend::Backend;
use crate::cli::{GoalAction, HeadlessCommand};
use crate::config::Config;
use crate::proto;
use crate::service::Caller;
pub use settings::{MidTurn, Settings};

pub async fn run(command: HeadlessCommand, dir: &Path, as_human: Option<&str>) -> Result<()> {
    match command {
        HeadlessCommand::Run(args) => run::run(dir, as_human, args).await,
        HeadlessCommand::Logs {
            agent,
            follow,
            pretty,
        } => log::print(&state::AgentDir::new(&agent)?.log_path(), follow, pretty).await,
        HeadlessCommand::Cancel { agent } => cancel(dir, as_human, agent).await,
        HeadlessCommand::Goal { agent, action } => goal(dir, as_human, agent, action).await,
        HeadlessCommand::Mcp {
            local_state_dir,
            auto_owner_reply,
            goal_dir,
            goal_max_wait,
        } => {
            let goal = match goal_dir {
                Some(dir) => Some(mcp::GoalTool {
                    dir,
                    max_wait: match goal_max_wait {
                        Some(s) => humantime::parse_duration(&s)
                            .map_err(|e| anyhow::anyhow!("invalid --goal-max-wait {s:?}: {e}"))?,
                        None => goal::DEFAULT_MAX_WAIT,
                    },
                }),
                None => None,
            };
            mcp::run(
                local_state_dir.as_deref().unwrap_or(dir),
                local_state_dir.is_some(),
                auto_owner_reply,
                goal,
            )
            .await
        }
    }
}

/// Send a control message to an agent its human owns.
async fn control(
    dir: &Path,
    as_human: Option<&str>,
    agent: String,
    body: String,
    control: proto::Control,
) -> Result<String> {
    let config = Config::load()?;
    let caller = Caller {
        human: config.human(as_human),
        ..Default::default()
    };
    let mut backend = Backend::open(&config, dir, caller).await?;
    let r = backend
        .send(proto::SendRequest {
            to: agent,
            body,
            control: Some(control.into()),
            reply_to: None,
            not_to_owner: false,
        })
        .await?;
    Ok(r.to)
}

/// Ask the agent's running `bbs headless run` to cancel its turn.
async fn cancel(dir: &Path, as_human: Option<&str>, agent: String) -> Result<()> {
    let body = "Cancel your running turn (bbs headless cancel).".to_string();
    let to = control(dir, as_human, agent, body, proto::Control::Cancel).await?;
    println!("Asked {to} to cancel its running turn.");
    Ok(())
}

/// Ask the agent's running `bbs headless run` to change its goal.
async fn goal(dir: &Path, as_human: Option<&str>, agent: String, action: GoalAction) -> Result<()> {
    let (body, kind, verb) = match action {
        GoalAction::Set {
            objective,
            until,
            for_,
        } => {
            // Check the deadline before sending anything.
            let deadline = until.or(for_).map(|w| deadline_body(&w)).transpose()?;
            let to = control(dir, as_human, agent, objective, proto::Control::GoalSet).await?;
            if let Some(body) = deadline {
                control(
                    dir,
                    as_human,
                    to.clone(),
                    body.clone(),
                    proto::Control::GoalDeadline,
                )
                .await?;
                println!("Asked {to}'s runner to set its goal, due {body}.");
            } else {
                println!("Asked {to}'s runner to set its goal.");
            }
            return Ok(());
        }
        GoalAction::Deadline { when } => (
            deadline_body(&when)?,
            proto::Control::GoalDeadline,
            "change the deadline of",
        ),
        GoalAction::Pause => (
            "Pause your goal.".into(),
            proto::Control::GoalPause,
            "pause",
        ),
        GoalAction::Resume => (
            "Resume your goal.".into(),
            proto::Control::GoalResume,
            "resume",
        ),
        GoalAction::Clear => (
            "Clear your goal.".into(),
            proto::Control::GoalClear,
            "clear",
        ),
    };
    let to = control(dir, as_human, agent, body, kind).await?;
    println!("Asked {to}'s runner to {verb} its goal.");
    Ok(())
}

/// A goal deadline control's body: an RFC 3339 time, a duration from now
/// ("8h"), or "none".
fn deadline_body(when: &str) -> Result<String> {
    let when = when.trim();
    if when.eq_ignore_ascii_case("none") {
        return Ok("none".into());
    }
    let at = match chrono::DateTime::parse_from_rfc3339(when) {
        Ok(at) => at.with_timezone(&chrono::Utc),
        Err(_) => {
            let span = humantime::parse_duration(when).with_context(|| {
                format!("{when:?} is not an RFC 3339 time, a duration such as 8h, or none")
            })?;
            chrono::Utc::now() + chrono::Duration::from_std(span).context("that is too long")?
        }
    };
    if at <= chrono::Utc::now() {
        bail!("the deadline {when} is not in the future");
    }
    Ok(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
