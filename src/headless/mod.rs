//! `bbs headless`: run a coding agent without a terminal, driven by its BBS
//! mail. See HEADLESS.md.

mod acp;
mod log;
mod mcp;
mod run;
mod settings;
mod state;

use std::path::Path;

use anyhow::Result;

use crate::cli::HeadlessCommand;
pub use settings::MidTurn;

pub async fn run(command: HeadlessCommand, dir: &Path, as_human: Option<&str>) -> Result<()> {
    match command {
        HeadlessCommand::Run(args) => run::run(dir, as_human, args).await,
        HeadlessCommand::Logs { agent, follow } => {
            log::print(&state::AgentDir::new(&agent)?.log_path(), follow).await
        }
        HeadlessCommand::Cancel { agent } => {
            let _ = agent;
            todo!("send a cancel control message (server capability agent-control.v1)")
        }
        HeadlessCommand::Mcp => mcp::run(dir).await,
    }
}
