//! Local account administration, authorized by access to the server state.

use crate::{
    cli::{AdminCommand, AdminHumanCommand, AdminIntegrationCommand},
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
        AdminCommand::Integration { command } => integration(&mut conn, command)?,
    }
    Ok(())
}

fn integration(conn: &mut rusqlite::Connection, command: &AdminIntegrationCommand) -> Result<()> {
    use service::integration as admin;
    match command {
        AdminIntegrationCommand::Add { id } => println!("{}", admin::admin_add(conn, id)?),
        AdminIntegrationCommand::Rotate { id } => println!("{}", admin::admin_rotate(conn, id)?),
        AdminIntegrationCommand::Revoke { id } => {
            if admin::admin_revoke(conn, id)? {
                println!(
                    "Revoked the credential for integration {id}; it is disabled until \
                     `bbs admin integration rotate {id}` issues a new one."
                );
            } else {
                println!("Integration {id} had no credential; it is disabled.");
            }
        }
        AdminIntegrationCommand::Publish { id, state } => {
            let on = state == "on";
            let changed = admin::admin_publish(conn, id, on)?;
            let now = if on {
                "publishes every active human and agent"
            } else {
                "publishes nothing, and remote sends are refused"
            };
            let already = if changed { "" } else { " already" };
            println!("Integration {id}{already} {now}.");
        }
        AdminIntegrationCommand::Unbind { namespace } => {
            match admin::admin_unbind(conn, namespace)? {
                Some(id) => println!(
                    "Namespace {namespace} is no longer bound to integration {id}; the next \
                 integration to import from it or send to it takes it."
                ),
                None => println!("Namespace {namespace} is not bound to an integration."),
            }
        }
        AdminIntegrationCommand::List => {
            let rows = admin::admin_list(conn)?;
            if rows.is_empty() {
                println!("No integrations.");
            }
            for row in rows {
                let handoffs = row
                    .outbox
                    .iter()
                    .map(|(state, n)| format!("{n} {state}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{}: {}, publication {} (revision {}), credential {}; handoffs: {}; \
                     namespaces: {}",
                    row.id,
                    if row.enabled { "enabled" } else { "disabled" },
                    if row.publish_entities { "on" } else { "off" },
                    row.publication_revision,
                    if row.has_credential { "issued" } else { "none" },
                    if handoffs.is_empty() {
                        "none".to_string()
                    } else {
                        handoffs
                    },
                    if row.namespaces.is_empty() {
                        "none".to_string()
                    } else {
                        row.namespaces.join(", ")
                    },
                );
            }
        }
    }
    Ok(())
}
