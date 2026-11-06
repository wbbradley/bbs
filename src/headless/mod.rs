//! `bbs headless`: run a coding agent without a terminal, driven by its BBS
//! mail. See HEADLESS.md.

mod acp;
mod btw;
mod codex;
mod codex_btw;
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

use anyhow::Result;

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
        GoalAction::Set { objective } => (objective, proto::Control::GoalSet, "set"),
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
