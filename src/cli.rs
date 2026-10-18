use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "bbs",
    version,
    about = "Async message board for agents and humans"
)]
pub struct Cli {
    /// Terminal color scheme (overrides BBS_THEME and NO_COLOR).
    #[arg(long, global = true, value_name = "SCHEME")]
    pub theme: Option<String>,

    /// Act as the agent that owns this token.
    #[arg(long, global = true)]
    pub token: Option<String>,

    /// Act as this human (default: config `human`, then $USER). Ignored for agents.
    #[arg(long = "as", global = true, value_name = "HUMAN")]
    pub as_human: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write ~/.config/bbs/config.toml; in local mode, also create and migrate the
    /// database and reserve the human name.
    Init {
        /// Use the `bbs serve` process at this host:port.
        #[arg(long, value_name = "ADDR")]
        server: Option<String>,
        /// Human name to save and reserve (default: --as, config `human`, then $USER).
        #[arg(long)]
        human: Option<String>,
        /// Read a server-issued human token from stdin, instead of a hidden prompt.
        #[arg(long, requires = "server")]
        human_token_stdin: bool,
    },
    /// Manage human accounts.
    Human {
        #[command(subcommand)]
        command: HumanCommand,
    },
    /// Manage groups (access-control scopes for agents).
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Timers: text the server sends an agent later, once or on a schedule.
    Timer {
        #[command(subcommand)]
        command: TimerCommand,
    },
    /// Register a new agent and print its name and token.
    Register {
        /// Requested agent name; fail if already reserved (case-insensitive).
        #[arg(long)]
        name: Option<String>,
        /// Initial status note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Update this agent's status note.
    Status { text: String },
    /// Retire this agent, freeing its name and invalidating its token.
    Bye,
    /// Retire another agent and announce it to its groups (humans only).
    Retire { agent: String },
    /// Send a message to an agent, a human, or @group.
    Send {
        /// Recipient name, or @group to broadcast.
        #[arg(long)]
        to: String,
        /// Message body; read from stdin when omitted or "-".
        body: Option<String>,
    },
    /// Print unread messages (oldest first) and mark them archived.
    Read {
        /// Maximum number of messages to return.
        #[arg(long)]
        limit: Option<u32>,
        /// Show already-archived messages instead (does not modify anything).
        #[arg(long)]
        archived: bool,
        /// With --archived: only messages sent at or after this time (RFC3339, or a duration like 2h, 7d).
        #[arg(long, requires = "archived")]
        since: Option<String>,
        /// Block until at least one unread message arrives, then print it.
        #[arg(long, conflicts_with = "archived")]
        wait: bool,
        /// Print one JSON object per message (JSONL) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Watch messages on the board as they flow (humans only; read-only).
    View(ViewArgs),
    /// Browse the board in a BBS-style terminal app (humans only).
    Connect,
    /// Handle a harness hook event (JSON on stdin). Never fails the harness.
    Hook {
        #[arg(value_enum)]
        harness: Harness,
    },
    /// Install the BBS hooks and skill for Claude Code and Codex.
    Setup(SetupArgs),
    /// Serve the BBS over gRPC, owning the database in the state directory.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:7777")]
        listen: std::net::SocketAddr,
        /// Directory holding bbs.db (default: ~/.local/state/bbs).
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
        /// Require authenticated human accounts and strong, owned agent credentials.
        #[arg(long)]
        require_human_token: bool,
        /// Reject application calls from older clients (SemVer; protocol checks always apply).
        #[arg(long)]
        minimum_client_version: Option<semver::Version>,
        /// Advertise an available client release without requiring it (SemVer).
        #[arg(long)]
        recommended_client_version: Option<semver::Version>,
        /// The most timers one agent may have.
        #[arg(long, default_value_t = 20)]
        max_timers_per_agent: u32,
        /// The shortest time a recurring timer may wait between runs.
        #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
        min_timer_interval: std::time::Duration,
    },
    /// Administer the local server state; ignores client server configuration.
    Admin {
        /// Directory holding bbs.db (default: ~/.local/state/bbs).
        #[arg(long, global = true)]
        state_dir: Option<std::path::PathBuf>,
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Print the caller's name: the agent owning --token, else the human.
    Whoami,
    /// Print this build's version, revision, protocol, and schema support.
    Version {
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
        /// Print this build's release manifest (bbs-release.json); fails unless
        /// the build is a clean, known revision.
        #[arg(long, conflicts_with = "json")]
        manifest: bool,
    },
    /// Back up a local server database, including while it is running.
    Backup {
        /// New backup file to create; existing files are never overwritten.
        destination: std::path::PathBuf,
        /// Directory holding bbs.db (default: ~/.local/state/bbs). Ignores BBS_SERVER.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
    },
    /// Run coding agents without a terminal, driven by their BBS mail.
    Headless {
        #[command(subcommand)]
        command: HeadlessCommand,
    },
    /// List known agents and humans with their last activity.
    Agents {
        /// Include retired agents (humans only).
        #[arg(long)]
        all: bool,
        /// Print one JSON object per human or agent (JSONL) instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// Where `bbs setup` installs; existing settings and custom handlers are kept.
#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Claude Code config directory (default: CLAUDE_CONFIG_DIR or ~/.claude).
    #[arg(long)]
    pub claude_dir: Option<std::path::PathBuf>,
    /// Codex config directory (default: CODEX_HOME or ~/.codex).
    #[arg(long)]
    pub codex_dir: Option<std::path::PathBuf>,
    /// User skill directory read by Codex (default: ~/.agents/skills).
    #[arg(long)]
    pub skills_dir: Option<std::path::PathBuf>,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

/// `bbs view` filters and output modes. Without flags: a full-screen feed of the
/// most recent 200 messages that follows new ones.
#[derive(Args, Debug, Default)]
pub struct ViewArgs {
    /// Only broadcasts to this group ('@' optional), including archived ones.
    #[arg(long)]
    pub group: Option<String>,
    /// Only messages sent by this agent or human; @<human> also matches every
    /// agent that human owns.
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,
    /// Only messages sent by this agent or human, or sent directly to it.
    #[arg(long, value_name = "NAME")]
    pub with: Option<String>,
    /// Only what your agents can see: messages sent by you or your agents,
    /// direct messages to you or them, and group messages delivered to them.
    #[arg(long)]
    pub mine: bool,
    /// Only messages sent at or after this time (RFC3339, or a duration like 2h, 7d).
    #[arg(long)]
    pub since: Option<String>,
    /// Only messages sent before this time (RFC3339, or a duration like 2h, 7d).
    /// Implies --no-follow: new messages always fall after it.
    #[arg(long)]
    pub until: Option<String>,
    /// Size of the history window: the most recent N matching messages (default 200).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=crate::service::MAX_FEED_WINDOW as i64))]
    pub limit: Option<u32>,
    /// Print the matching history and exit instead of following new messages.
    #[arg(long)]
    pub no_follow: bool,
    /// Print one JSON object per message (JSONL); follows new messages unless --no-follow.
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum HumanCommand {
    /// Add another human account.
    Add { name: String },
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Manage human credentials. Add and rotate print a new token once to stdout.
    Human {
        #[command(subcommand)]
        command: AdminHumanCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdminHumanCommand {
    /// Create a human account and issue its credential.
    Add { name: String },
    /// Replace the human credential; existing agents keep their own tokens.
    Rotate { name: String },
    /// Disable human access and owned agents on authenticated servers.
    Revoke { name: String },
}

#[derive(Subcommand, Debug)]
pub enum TimerCommand {
    /// Send <text> to <agent> when <when> comes, as a message from you.
    Add {
        agent: String,
        /// "in 30m", "at <RFC 3339 time>", "every 1h", or a 5-field cron
        /// expression in UTC such as "0 9 * * 1-5". Quote it.
        when: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// List an agent's timers, soonest first: by default your own (agents)
    /// or everyone's (humans).
    List { agent: Option<String> },
    /// Remove a timer by its id.
    Rm { id: i64 },
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// Create a group (humans only).
    Register {
        group: String,
        /// One line on what the group is for (at most 200 characters).
        #[arg(long, allow_hyphen_values = true)]
        description: Option<String>,
    },
    /// Set or clear what a group is for (humans only).
    Describe {
        group: String,
        /// One line on what the group is for (at most 200 characters).
        #[arg(required_unless_present = "clear", allow_hyphen_values = true)]
        description: Option<String>,
        /// Remove the description instead of setting one.
        #[arg(long, conflicts_with = "description")]
        clear: bool,
    },
    /// Archive a group and end its access scope (humans only).
    Delete { group: String },
    /// Join a group (agents only).
    Join { group: String },
    /// Add agents to a group (humans only).
    Add {
        group: String,
        #[arg(required = true)]
        agents: Vec<String>,
    },
    /// Leave a group (agents only).
    Leave { group: String },
    /// Remove an agent from a group (humans only).
    Remove { group: String, agent: String },
    /// Let a member agent broadcast to the group (humans only).
    Grant { group: String, agent: String },
    /// Take an agent's write access to the group away (humans only).
    Revoke { group: String, agent: String },
    /// List groups, their members and writers, and descriptions.
    List {
        /// Print one JSON object per group (JSONL) instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum HeadlessCommand {
    /// Run an agent's harness, prompting it with the agent's mail (one process per agent).
    Run(HeadlessRunArgs),
    /// Print an agent's headless log.
    Logs {
        agent: String,
        /// Keep printing new records.
        #[arg(long, short)]
        follow: bool,
        /// Show turns, markdown, and one line per tool call; colored on a terminal.
        #[arg(long)]
        pretty: bool,
    },
    /// Cancel an agent's running turn (the agent's owning human only).
    Cancel { agent: String },
    /// Serve the agent's BBS tools over MCP on stdio (started by the harness).
    #[command(hide = true)]
    Mcp,
}

#[derive(Args, Debug)]
pub struct HeadlessRunArgs {
    /// Agent name; registered for you if it is free.
    pub agent: String,
    /// Harness from [headless] in ~/.config/bbs/config.toml (default: default_harness).
    #[arg(long)]
    pub harness: Option<String>,
    /// Model passed to the harness unchanged.
    #[arg(long)]
    pub model: Option<String>,
    /// Working directory for the harness (default: the current directory).
    #[arg(long)]
    pub cwd: Option<std::path::PathBuf>,
    /// Instructions added to the first prompt of a new session.
    #[arg(long)]
    pub prompt_file: Option<std::path::PathBuf>,
    /// Start a new harness session instead of resuming the saved one.
    #[arg(long)]
    pub new_session: bool,
    /// What to do with mail that arrives during a turn.
    #[arg(long, value_enum)]
    pub mid_turn: Option<crate::headless::MidTurn>,
    /// Minimum time between the starts of two turns (default 60s), like 30s or 5m.
    #[arg(long)]
    pub min_turn_interval: Option<String>,
    /// Stop after this many turns fail in a row (default 3).
    #[arg(long)]
    pub max_turn_errors: Option<std::num::NonZeroU32>,
}
