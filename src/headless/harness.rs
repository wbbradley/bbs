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
    Text(super::outbox::Source, String),
    TextEnd(super::outbox::Source, Option<String>),
    /// Provider metadata changed; read the latest snapshot. Never an ACK.
    EffortChanged,
    OwnerInput(super::input::Form),
    InputClosed(String),
    ContextChanged(Option<crate::proto::ContextUsage>),
    EffortOutcome(crate::proto::EffortOutcome),
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
    pub input_enabled: bool,
}

/// A saved session to resume.
#[derive(Clone, Copy)]
pub struct Resume<'a> {
    pub id: &'a str,
    /// Codex: the thread was started with the BBS tools as plain functions,
    /// which it keeps. Otherwise it was opened through ACP and has them only
    /// from an MCP server, which must be supplied again.
    pub dynamic_tools: bool,
    /// Fail rather than open a new session when this one cannot be resumed.
    pub strict: bool,
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
        resume: Option<Resume<'_>>,
    ) -> Result<Started> {
        Ok(match config.kind {
            HarnessType::Acp => {
                let (acp, process, events, warnings) =
                    Acp::start(config, cwd, stderr, &bbs.mcp, resume, bbs.input_enabled).await?;
                (Harness::Acp(acp), process, events, warnings)
            }
            HarnessType::CodexAppServer => {
                let (codex, process, events) = Codex::start(
                    config,
                    cwd,
                    stderr,
                    bbs.tools,
                    &bbs.mcp,
                    resume,
                    bbs.input_enabled,
                )
                .await?;
                (Harness::Codex(codex), process, events, Vec::new())
            }
        })
    }

    /// Whether the session's thread holds the BBS tools as plain functions.
    pub fn dynamic_tools(&self) -> bool {
        match self {
            Harness::Acp(_) => false,
            Harness::Codex(codex) => codex.dynamic_tools,
        }
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

    pub fn effort_state(&self) -> crate::proto::EffortState {
        match self {
            Self::Acp(a) => a.effort_state(),
            Self::Codex(c) => c.effort_state(),
        }
    }

    pub fn side_supported(&self) -> bool {
        matches!(self, Self::Codex(_))
    }
    pub async fn finish_side(&self) -> bool {
        match self {
            Self::Codex(c) => c.finish_side().await,
            Self::Acp(_) => true,
        }
    }

    pub fn side_text(&self) -> String {
        match self {
            Self::Codex(c) => c.side_text(),
            Self::Acp(_) => String::new(),
        }
    }
    pub fn side_retained(&self) -> bool {
        match self {
            Self::Codex(c) => c.side_retained(),
            Self::Acp(_) => false,
        }
    }
    #[cfg(test)]
    pub async fn side_question(
        &self,
        question: &str,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> super::codex_btw::Outcome {
        self.side_turn(question, super::codex_btw::Mode::OneShot, cancel)
            .await
    }
    pub async fn side_turn(
        &self,
        question: &str,
        mode: super::codex_btw::Mode,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> super::codex_btw::Outcome {
        match self {
            Self::Codex(c) => c.side_turn(question, mode, cancel).await,
            Self::Acp(_) => super::codex_btw::Outcome {
                status: super::codex_btw::Status::Unsupported,
                answer: String::new(),
                detail: "ACP side-fork isolation has not been verified".into(),
                disable: true,
                retained: false,
            },
        }
    }

    pub fn input_pending(&self, key: &str) -> bool {
        match self {
            Self::Acp(a) => a.inputs.pending(key),
            Self::Codex(c) => c.inputs().pending(key),
        }
    }

    pub fn enable_input(&self, enabled: bool) {
        match self {
            Self::Acp(a) => a.inputs.enable(enabled),
            Self::Codex(c) => c.inputs().enable(enabled),
        }
    }

    pub fn cancel_input(&self) {
        match self {
            Self::Acp(a) => a.cancel_input(),
            Self::Codex(c) => c.cancel_input(),
        }
    }

    pub async fn answer_input(
        &self,
        key: &str,
        action: crate::proto::AnswerAction,
        answers: &[crate::proto::InputAnswer],
    ) -> Result<()> {
        match self {
            Self::Acp(a) => a.answer_input(key, action, answers).await,
            Self::Codex(c) => c.answer_input(key, action, answers).await,
        }
    }

    /// Run one turn; returns how it ended.
    pub async fn prompt(
        &self,
        text: &str,
        effort: Option<crate::proto::EffortRequest>,
    ) -> Result<String> {
        match self {
            Harness::Acp(acp) => acp.prompt(text, effort).await,
            Harness::Codex(codex) => codex.prompt(text, effort).await,
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

/// Definite rejection before a prompt was accepted. Its mail can be retried.
#[derive(Debug)]
pub struct PromptNotStarted(pub String);
impl std::fmt::Display for PromptNotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PromptNotStarted {}

/// A configuration/start request may still be executing. Stop rather than
/// starting another turn or allowing a late setter to change an active turn.
#[derive(Debug)]
pub struct EffortUnconfirmed {
    pub message: String,
    pub prompt_sent: bool,
}
impl std::fmt::Display for EffortUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for EffortUnconfirmed {}
