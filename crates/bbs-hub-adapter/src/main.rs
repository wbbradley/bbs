//! `bbs-hub-adapter`: federate a BBS installation with Hub.

use std::path::PathBuf;

use anyhow::Result;
use bbs_hub_adapter::config::Config;
use bbs_hub_adapter::store::{Direction, Store};
use bbs_hub_adapter::{Adapter, keys};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bbs-hub-adapter", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the adapter: connect to BBS's integration stream and the Hub relay.
    Run {
        /// The adapter's configuration file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Generate an installation key: write its seed (owner-only, never
    /// overwriting) and print only its public key, as hex, for the
    /// namespace authority to certify.
    Keygen {
        /// The seed file to create.
        #[arg(long)]
        out: PathBuf,
    },
    /// Show the store: trust, the relay session and handoffs by state.
    Status {
        /// The adapter's configuration file.
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Run { config } => {
            let cfg = Config::load(&config)?;
            let adapter = Adapter::from_config(&cfg)?;
            tracing::info!(
                namespace = cfg.namespace,
                installation = cfg.hub.installation_id,
                integration = cfg.bbs.integration_id,
                "adapter starting"
            );
            adapter.run(shutdown()).await
        }
        Command::Keygen { out } => {
            let public = keys::generate(&out)?;
            println!("{}", data_encoding::HEXLOWER.encode(public.as_bytes()));
            Ok(())
        }
        Command::Status { config } => status(&Config::load(&config)?),
    }
}

async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

fn status(cfg: &Config) -> Result<()> {
    let store = Store::open_read_only(&cfg.store)?;
    println!(
        "namespace {} (installation {})",
        cfg.namespace, cfg.hub.installation_id
    );
    match store.session()? {
        Some((conn, relay, ready, ended)) => println!(
            "relay session {conn} with {relay}: {}",
            match (ready, ended) {
                (_, true) => "ended",
                (true, false) => "route registered",
                (false, false) => "connecting",
            }
        ),
        None => println!("relay session: never connected"),
    }
    for b in store.bindings()? {
        println!(
            "trusted namespace {} revision {}{}",
            b.namespace,
            b.revision,
            if b.disabled { " (disabled)" } else { "" }
        );
    }
    match store.catalog_revision()? {
        Some(r) => println!("BBS catalog revision {r}"),
        None => println!("BBS catalog: none yet"),
    }
    for (dir, name) in [
        (Direction::Outbound, "outbound (BBS to Hub)"),
        (Direction::Inbound, "inbound (Hub to BBS)"),
    ] {
        let counts = store.counts(dir)?;
        let text: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
        println!(
            "{name}: {}",
            if text.is_empty() {
                "none".to_string()
            } else {
                text.join(", ")
            }
        );
    }
    Ok(())
}
