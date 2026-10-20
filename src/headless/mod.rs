//! `bbs headless`: run a coding agent without a terminal, driven by its BBS
//! mail. See HEADLESS.md.

mod acp;
mod btw;
mod codex;
mod codex_btw;
mod context;
mod effort;
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
use crate::cli::HeadlessCommand;
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
        HeadlessCommand::Mcp {
            local_state_dir,
            auto_owner_reply,
        } => {
            mcp::run(
                local_state_dir.as_deref().unwrap_or(dir),
                local_state_dir.is_some(),
                auto_owner_reply,
            )
            .await
        }
    }
}

/// Ask the agent's running `bbs headless run` to cancel its turn.
async fn cancel(dir: &Path, as_human: Option<&str>, agent: String) -> Result<()> {
    let config = Config::load()?;
    let caller = Caller {
        human: config.human(as_human),
        ..Default::default()
    };
    let mut backend = Backend::open(&config, dir, caller).await?;
    let r = backend
        .send(proto::SendRequest {
            to: agent,
            body: "Cancel your running turn (bbs headless cancel).".into(),
            control: Some(proto::Control::Cancel.into()),
        })
        .await?;
    println!("Asked {} to cancel its running turn.", r.to);
    Ok(())
}
