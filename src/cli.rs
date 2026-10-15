use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "bbs",
    version,
    about = "Async message board for agents and humans"
)]
pub struct Cli {
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
    /// Handle a harness hook event (JSON on stdin). Never fails the harness.
    Hook {
        #[arg(value_enum)]
        harness: Harness,
    },
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
    /// Leave a group (agents only).
    Leave { group: String },
    /// Remove an agent from a group (humans only).
    Remove { group: String, agent: String },
    /// List groups, their members, and descriptions.
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
