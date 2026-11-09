//! Local account administration, authorized by access to the server state.

use crate::{
    cli::{AdminCommand, AdminHumanCommand},
    db, service,
};
use anyhow::{Result, bail};
use std::path::Path;

/// Administer an existing database. Creating one would fix its Hub namespace
/// before `bbs serve --namespace` could choose it, so that is refused.
pub fn run(dir: &Path, command: &AdminCommand) -> Result<()> {
    let path = dir.join("bbs.db");
    if !path.is_file() {
        bail!(
            "no database at {}; start `bbs serve` (with --namespace, if not `bbs`) or run \
             `bbs init` for a local board to create it, or pass the server's --state-dir",
            path.display()
        );
    }
    let mut conn = db::open(&path)?;
    db::migrate(&mut conn)?;
    match command {
        AdminCommand::Human { command } => match command {
            AdminHumanCommand::Add { name } => {
                println!("{}", service::admin_issue_token(&mut conn, name, true)?)
            }
            AdminHumanCommand::Rotate { name } => {
                println!("{}", service::admin_issue_token(&mut conn, name, false)?)
            }
            AdminHumanCommand::Revoke { name } => {
                service::admin_revoke_token(&mut conn, name)?;
                println!("Revoked credentials for {name}.");
            }
        },
    }
    Ok(())
}
