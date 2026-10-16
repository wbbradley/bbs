//! The harness a headless agent runs in: an ACP agent, or Codex's app-server.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::acp::{Acp, McpServer};
use super::codex::Codex;
use super::mcp::Tools;
use super::rpc::Process;
use super::settings::{HarnessConfig, HarnessType};

/// Longest wait for the harness to initialize and open a session.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest tool input kept in an event.
pub const MAX_INPUT_CHARS: usize = 2000;

/// What the harness reported during a session.
#[derive(Debug)]
pub enum Event {
    Text(String),
    Thought(String),
    /// A tool call started or changed: id, and the fields worth logging.
    Tool(String, Value),
    /// A permission request and the option chosen for it.
    Permission(Value),
    /// A problem the harness reported outside a turn's result.
    Warning(String),
    /// A session update this client does not interpret, by kind.
    Other(String),
    /// The harness's stdout closed.
    Exited,
}

/// How the harness reaches the agent's BBS tools.
pub struct BbsTools {
    /// For ACP: the MCP server the harness starts.
    pub mcp: McpServer,
    /// For Codex: tools answered in this process.
    pub tools: Tools,
}

#[derive(Clone)]
pub enum Harness {
    Acp(Acp),
    Codex(Codex),
}

pub type Started = (
    Harness,
    Process,
    mpsc::UnboundedReceiver<Event>,
    Vec<String>,
);

impl Harness {
    /// Start the harness and open a session, resuming `resume` when it can.
    pub async fn start(
        config: &HarnessConfig,
        cwd: &Path,
        stderr: &Path,
        bbs: BbsTools,
        resume: Option<&str>,
    ) -> Result<Started> {
        Ok(match config.kind {
            HarnessType::Acp => {
                let (acp, process, events, warnings) =
                    Acp::start(config, cwd, stderr, &bbs.mcp, resume).await?;
                (Harness::Acp(acp), process, events, warnings)
            }
            HarnessType::CodexAppServer => {
                let (codex, process, events) =
                    Codex::start(config, cwd, stderr, bbs.tools, resume).await?;
                (Harness::Codex(codex), process, events, Vec::new())
            }
        })
    }

    pub fn session_id(&self) -> &str {
        match self {
            Harness::Acp(acp) => acp.session_id(),
            Harness::Codex(codex) => codex.thread_id(),
        }
    }

    pub fn session_resumed(&self) -> bool {
        match self {
            Harness::Acp(acp) => acp.session_resumed,
            Harness::Codex(codex) => codex.resumed,
        }
    }

    /// Whether mail can be added to a running turn. ACP has no way to.
    pub fn can_steer(&self) -> bool {
        matches!(self, Harness::Codex(_))
    }

    /// Run one turn; returns how it ended.
    pub async fn prompt(&self, text: &str) -> Result<String> {
        match self {
            Harness::Acp(acp) => acp.prompt(text).await,
            Harness::Codex(codex) => codex.prompt(text).await,
        }
    }

    /// Add `text` to the running turn.
    pub async fn steer(&self, text: &str) -> Result<()> {
        match self {
            Harness::Acp(_) => bail!("ACP harnesses cannot steer"),
            Harness::Codex(codex) => codex.steer(text).await,
        }
    }

    /// Ask the harness to stop the running turn.
    pub async fn cancel(&self) -> Result<()> {
        match self {
            Harness::Acp(acp) => acp.cancel().await,
            Harness::Codex(codex) => codex.cancel().await,
        }
    }
}

/// `s`, cut to `max` characters with an ellipsis.
pub fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        s = s.chars().take(max).collect();
        s.push('…');
    }
    s
}
