mod admin;
// The shared core, under the names the CLI's modules have always used.
use bbs_core::{auth, backend, db, proto, protocol, release, service};
mod app;
mod backup;
mod blockers;
mod cli;
mod config;
mod connect;
mod headless;
mod hook;
mod serve;
mod setup;
mod ui;
mod view;

use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Mutex;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Log to ~/bbs.log, or to stderr when `BBS_LOG_STDERR=1` (containers). Logging is
/// best-effort: failure to open the file never blocks the CLI.
fn init_logging() {
    let filter = EnvFilter::try_from_env("BBS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter);
    if std::env::var_os("BBS_LOG_STDERR").is_some_and(|v| v == "1") {
        fmt.with_writer(std::io::stderr).init();
        return;
    }
    let Ok(home) = app::home_dir() else { return };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("bbs.log"))
    else {
        return;
    };
    fmt.with_writer(Mutex::new(file)).init();
}

#[tokio::main]
async fn main() -> ExitCode {
    release::init(bbs_core::identity!());
    let cli = cli::Cli::parse();
    if let Some(cli::Command::Serve { args }) = &cli.command {
        return serve::exec(args);
    }
    // Handled before logging, which creates ~/bbs.log, so they have no side effects.
    // Bare `bbs` outside a terminal behaves as it did before defaulting to
    // connect: usage on stderr, exit 2, no log file or database touched.
    if cli.command.is_none() && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        use clap::CommandFactory;
        cli::Cli::command()
            .error(
                clap::error::ErrorKind::MissingSubcommand,
                "outside a terminal, a subcommand is required",
            )
            .exit();
    }
    let result = if let Some(cli::Command::Version { json, manifest }) = cli.command {
        release::print(json, manifest)
    } else if let Some(cli::Command::Setup(args)) = &cli.command {
        setup::run(args)
    } else {
        init_logging();
        app::run(cli).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("bbs: {err:#}");
            ExitCode::FAILURE
        }
    }
}
