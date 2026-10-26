//! `bbs-server`: what `bbs serve` runs.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use bbs_core::{protocol, release, service};
use bbs_server::serve;

/// Serve the BBS over gRPC, owning the database in the state directory.
#[derive(Parser)]
#[command(name = "bbs-server", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: Serve,
}

#[derive(Subcommand)]
enum Command {
    /// Print this build's version, revision and protocol.
    Version {
        #[arg(long)]
        json: bool,
        /// Print the `bbs-release.json` a published release attaches.
        #[arg(long, conflicts_with = "json")]
        manifest: bool,
    },
}

#[derive(clap::Args)]
struct Serve {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:7777")]
    listen: std::net::SocketAddr,
    /// Also serve the web UI for humans on this address (for example
    /// 127.0.0.1:7778). Humans sign in with their name and human token.
    /// Needs a build with the `web` feature.
    #[arg(long, value_name = "ADDR")]
    web: Option<std::net::SocketAddr>,
    /// The web UI's origin as browsers reach it, such as
    /// https://bbs.example.com behind a TLS proxy. Required when --web is
    /// not a loopback address. An https origin marks the session cookie
    /// Secure; an http one serves sign-in and sessions in plain text.
    #[arg(long, value_name = "URL", requires = "web")]
    web_origin: Option<String>,
    /// Directory holding bbs.db (default: ~/.local/state/bbs).
    #[arg(long)]
    state_dir: Option<PathBuf>,
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
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

/// Log to ~/bbs.log, or to stderr when `BBS_LOG_STDERR=1` (containers), as
/// `bbs` does. Logging is best-effort: failure to open the file never stops
/// the server.
fn init_logging() {
    let filter = EnvFilter::try_from_env("BBS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter);
    if std::env::var_os("BBS_LOG_STDERR").is_some_and(|v| v == "1") {
        fmt.with_writer(std::io::stderr).init();
        return;
    }
    let Ok(home) = home_dir() else { return };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("bbs.log"))
    else {
        return;
    };
    fmt.with_writer(Mutex::new(file)).init();
}

async fn run(args: Serve) -> Result<()> {
    let dir = match args.state_dir {
        Some(dir) => dir,
        None => home_dir()?.join(".local/state/bbs"),
    };
    serve::run(
        args.listen,
        args.web.map(|addr| serve::Web {
            addr,
            origin: args.web_origin,
        }),
        &dir,
        args.require_human_token,
        protocol::Policy::new(args.minimum_client_version, args.recommended_client_version)?,
        service::TimerLimits {
            max_per_agent: args.max_timers_per_agent,
            min_interval: args.min_timer_interval,
        },
    )
    .await
}

#[tokio::main]
async fn main() -> ExitCode {
    release::init(bbs_core::identity!());
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Version { json, manifest }) => release::print(json, manifest),
        None => {
            init_logging();
            run(cli.serve).await
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("bbs-server: {err:#}");
            ExitCode::FAILURE
        }
    }
}
