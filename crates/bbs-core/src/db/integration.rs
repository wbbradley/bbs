//! Storage for Hub's integration stream (from the Hub model, schema 27): integrations and their
//! credentials, the publication catalog and its revisions, foreign entities
//! and addresses, imported messages and the outbound handoff outbox.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::entity::NATIVE_ADDRESS;

/// One configured integration, for `bbs admin integration list`.
#[derive(Debug, PartialEq, Eq)]
pub struct IntegrationRow {
    pub id: String,
    pub enabled: bool,
    pub publish_entities: bool,
    pub has_credential: bool,
    pub publication_revision: i64,
    /// Outbox rows by state, in state order.
    pub outbox: Vec<(String, i64)>,
    /// Other installations' namespaces bound to it, in order.
    pub namespaces: Vec<String>,
}

pub fn exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM bbs_integrations WHERE id = ?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// Create an enabled integration with publication off.
pub fn insert(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("INSERT INTO bbs_integrations (id) VALUES (?1)", [id])?;
    Ok(())
}

/// Store the hash of an integration's credential, replacing any earlier one,
/// and enable the integration.
pub fn set_token(conn: &Connection, id: &str, hash: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO integration_tokens (integration_id, token_hash) VALUES (?1, ?2)
         ON CONFLICT(integration_id) DO UPDATE SET token_hash = excluded.token_hash",
        params![id, hash],
    )?;
    conn.execute(
        "UPDATE bbs_integrations SET enabled = 1 WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Delete an integration's credential and disable it; returns whether it had
/// a credential.
pub fn revoke_token(conn: &Connection, id: &str) -> Result<bool> {
    let had = conn.execute(
        "DELETE FROM integration_tokens WHERE integration_id = ?1",
        [id],
    )? > 0;
    conn.execute(
        "UPDATE bbs_integrations SET enabled = 0 WHERE id = ?1",
        [id],
    )?;
    Ok(had)
}

/// The enabled integration whose credential hashes to `hash`.
pub fn by_token_hash(conn: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT i.id FROM integration_tokens t JOIN bbs_integrations i ON i.id = t.integration_id
             WHERE t.token_hash = ?1 AND i.enabled = 1",
            [hash],
            |r| r.get(0),
        )
        .optional()?)
}

/// Turn publication on or off; returns whether it changed.
pub fn set_publish(conn: &Connection, id: &str, on: bool) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE bbs_integrations SET publish_entities = ?2
         WHERE id = ?1 AND publish_entities != ?2",
        params![id, on],
    )? > 0)
}

pub fn list(conn: &Connection) -> Result<Vec<IntegrationRow>> {
    let mut rows: Vec<IntegrationRow> = conn
        .prepare(
            "SELECT i.id, i.enabled, i.publish_entities, t.integration_id IS NOT NULL,
                    i.publication_revision
             FROM bbs_integrations i
             LEFT JOIN integration_tokens t ON t.integration_id = i.id
             ORDER BY i.id",
        )?
        .query_map([], |r| {
            Ok(IntegrationRow {
                id: r.get(0)?,
                enabled: r.get(1)?,
                publish_entities: r.get(2)?,
                has_credential: r.get(3)?,
                publication_revision: r.get(4)?,
                outbox: Vec::new(),
                namespaces: Vec::new(),
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut counts = conn.prepare(
        "SELECT state, count(*) FROM integration_outbox WHERE integration_id = ?1
         GROUP BY state ORDER BY state",
    )?;
    let mut namespaces = conn.prepare(
        "SELECT namespace FROM integration_namespaces WHERE integration_id = ?1
         ORDER BY namespace",
    )?;
    for row in &mut rows {
        row.outbox = counts
            .query_map([&row.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        row.namespaces = namespaces
            .query_map([&row.id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
    }
    Ok(rows)
}

/// Whether `id` publishes its entities; None when it does not exist or is
/// disabled.
pub fn publishes(conn: &Connection, id: &str) -> Result<Option<bool>> {
    Ok(conn
        .query_row(
            "SELECT publish_entities FROM bbs_integrations WHERE id = ?1 AND enabled = 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Integrations a remote send may use: enabled, with a live credential. Each
/// with its publication switch, by id.
pub fn usable(conn: &Connection) -> Result<Vec<(String, bool)>> {
    Ok(conn
        .prepare(
            "SELECT i.id, i.publish_entities FROM bbs_integrations i
             JOIN integration_tokens t ON t.integration_id = i.id
             WHERE i.enabled = 1 ORDER BY i.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

/// The integration another installation's `namespace` is bound to: the first
/// that imported from it or sent to it.
pub fn namespace_integration(conn: &Connection, namespace: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT integration_id FROM integration_namespaces WHERE namespace = ?1",
            [namespace],
            |r| r.get(0),
        )
        .optional()?)
}

/// Clear `namespace`'s binding; returns the integration it was bound to.
pub fn unbind_namespace(conn: &Connection, namespace: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "DELETE FROM integration_namespaces WHERE namespace = ?1 RETURNING integration_id",
            [namespace],
            |r| r.get(0),
        )
        .optional()?)
}

/// Bind `namespace` to `integration` unless it is bound already.
pub fn bind_namespace(conn: &Connection, namespace: &str, integration: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO integration_namespaces (namespace, integration_id) VALUES (?1, ?2)
         ON CONFLICT(namespace) DO NOTHING",
        params![namespace, integration],
    )?;
    Ok(())
}

/// Bumped by every change to a native entity (migration 27's triggers).
pub fn catalog_epoch(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT catalog_epoch FROM bbs_installation", [], |r| {
            r.get(0)
        })?,
    )
}

/// Whether a known foreign entity was last observed retired.
pub fn foreign_retired(conn: &Connection, namespace: &str, entity_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT lifecycle = 'retired' FROM entities WHERE namespace = ?1 AND entity_id = ?2",
            params![namespace, entity_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// The revision of `id`'s publication snapshot with entities digesting to
/// `digest`: the stored one when the digest matches, else the next one, which
/// is stored with the new digest.
pub fn publication_revision(conn: &Connection, id: &str, digest: &str) -> Result<u64> {
    conn.execute(
        "UPDATE bbs_integrations
         SET publication_revision = publication_revision + 1, publication_digest = ?2
         WHERE id = ?1 AND publication_digest IS NOT ?2",
        params![id, digest],
    )?;
    let revision: i64 = conn.query_row(
        "SELECT publication_revision FROM bbs_integrations WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(revision as u64)
}

// ---- the publication catalog -----------------------------------------------

/// A native human or agent as BBS publishes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub entity_id: String,
    pub kind: String,
    pub display_name: String,
}

/// Active native humans and agents with an active native address, by entity
/// ID after `after`, at most `limit` (negative: all). Groups are not published.
pub fn published(conn: &Connection, after: &str, limit: i64) -> Result<Vec<Published>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT e.entity_id, e.kind, e.display_name
             FROM entities e
             JOIN entity_local_bindings b ON b.entity_id = e.id
             JOIN entity_addresses a ON a.entity_id = e.id AND a.address_id = '{NATIVE_ADDRESS}'
               AND a.lifecycle = 'active'
             WHERE e.lifecycle = 'active' AND b.group_id IS NULL AND e.entity_id > ?1
             ORDER BY e.entity_id LIMIT ?2"
        ))?
        .query_map(params![after, limit], |r| {
            Ok(Published {
                entity_id: r.get(0)?,
                kind: r.get(1)?,
                display_name: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// One published entity by its exact ID.
pub fn published_entity(conn: &Connection, entity_id: &str) -> Result<Option<Published>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT e.entity_id, e.kind, e.display_name
                 FROM entities e
                 JOIN entity_local_bindings b ON b.entity_id = e.id
                 JOIN entity_addresses a ON a.entity_id = e.id
                   AND a.address_id = '{NATIVE_ADDRESS}' AND a.lifecycle = 'active'
                 WHERE e.lifecycle = 'active' AND b.group_id IS NULL AND e.entity_id = ?1"
            ),
            [entity_id],
            |r| {
                Ok(Published {
                    entity_id: r.get(0)?,
                    kind: r.get(1)?,
                    display_name: r.get(2)?,
                })
            },
        )
        .optional()?)
}

// ---- native targets and foreign entities -----------------------------------

/// A native entity's address, as an incoming message names it.
#[derive(Debug)]
pub struct NativeTarget {
    /// The `entity_addresses` row.
    pub address: i64,
    pub human_id: Option<i64>,
    pub agent_id: Option<i64>,
    pub group_id: Option<i64>,
    /// Entity and address are both active.
    pub active: bool,
}

/// The native entity `entity_id` in `namespace` and its address `address_id`;
/// None when either does not exist or the entity is foreign.
pub fn native_target(
    conn: &Connection,
    namespace: &str,
    entity_id: &str,
    address_id: &str,
) -> Result<Option<NativeTarget>> {
    Ok(conn
        .query_row(
            "SELECT a.id, b.human_id, b.agent_id, b.group_id,
                    e.lifecycle = 'active' AND a.lifecycle = 'active'
             FROM entities e
             JOIN entity_local_bindings b ON b.entity_id = e.id
             JOIN entity_addresses a ON a.entity_id = e.id AND a.address_id = ?3
             WHERE e.namespace = ?1 AND e.entity_id = ?2",
            params![namespace, entity_id, address_id],
            |r| {
                Ok(NativeTarget {
                    address: r.get(0)?,
                    human_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    group_id: r.get(3)?,
                    active: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// The entity `entity_id` in another installation's `namespace`, recorded on
/// first sight with `kind` (`unknown` when nothing says) and its ID as display
/// name; a known `kind` replaces `unknown`. Returns its `entities` row.
pub fn foreign_entity(
    conn: &Connection,
    namespace: &str,
    entity_id: &str,
    kind: &str,
    now: &str,
) -> Result<i64> {
    if namespace == super::namespace(conn)? {
        bail!("{entity_id}@{namespace} is in this installation's own namespace");
    }
    conn.execute(
        "INSERT INTO entities (namespace, entity_id, kind, display_name, lifecycle, created_at)
         VALUES (?1, ?2, ?3, ?2, 'active', ?4)
         ON CONFLICT(namespace, entity_id) DO UPDATE SET kind = excluded.kind
           WHERE entities.kind = 'unknown' AND excluded.kind != 'unknown'",
        params![namespace, entity_id, kind, now],
    )?;
    let (id, native): (i64, bool) = conn.query_row(
        "SELECT e.id, EXISTS (SELECT 1 FROM entity_local_bindings b WHERE b.entity_id = e.id)
         FROM entities e WHERE e.namespace = ?1 AND e.entity_id = ?2",
        params![namespace, entity_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if native {
        bail!("{entity_id}@{namespace} is a native entity");
    }
    Ok(id)
}

/// A foreign entity's address `address_id`, reached through `integration`,
/// its namespace's integration: recorded on first sight, and moved to
/// `integration` when the namespace was rebound. Returns its
/// `entity_addresses` row.
pub fn foreign_address(
    conn: &Connection,
    entity: i64,
    address_id: &str,
    integration: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO entity_addresses (entity_id, address_id, label, lifecycle, is_default)
         VALUES (?1, ?2, '', 'active', 0)
         ON CONFLICT(entity_id, address_id) DO NOTHING",
        params![entity, address_id],
    )?;
    let address: i64 = conn.query_row(
        "SELECT id FROM entity_addresses WHERE entity_id = ?1 AND address_id = ?2",
        params![entity, address_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO address_delivery_bindings (address_id, handler, integration_id)
         VALUES (?1, 'integration', ?2)
         ON CONFLICT(address_id) DO UPDATE SET integration_id = excluded.integration_id
         WHERE handler = 'integration'",
        params![address, integration],
    )?;
    let bound: Option<String> = conn.query_row(
        "SELECT integration_id FROM address_delivery_bindings WHERE address_id = ?1",
        [address],
        |r| r.get(0),
    )?;
    if bound.is_none() {
        bail!("address {address} is native");
    }
    Ok(address)
}

/// Record what an integration observed about a foreign entity: its kind,
/// display name and lifecycle. Observations never touch native entities.
pub fn observe(
    conn: &Connection,
    namespace: &str,
    entity_id: &str,
    kind: &str,
    display_name: &str,
    retired: bool,
    now: &str,
) -> Result<()> {
    let lifecycle = if retired { "retired" } else { "active" };
    conn.execute(
        "INSERT INTO entities (namespace, entity_id, kind, display_name, lifecycle, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(namespace, entity_id) DO UPDATE
           SET kind = excluded.kind, display_name = excluded.display_name,
               lifecycle = excluded.lifecycle
           WHERE NOT EXISTS (SELECT 1 FROM entity_local_bindings b WHERE b.entity_id = entities.id)",
        params![namespace, entity_id, kind, display_name, lifecycle, now],
    )?;
    Ok(())
}

// ---- messages ----------------------------------------------------------------

/// The exact common bytes of the message with origin `(namespace, id)`; outer
/// None when there is no such message, inner None when it has no bytes.
pub fn origin_bytes(
    conn: &Connection,
    namespace: &str,
    message_id: &str,
) -> Result<Option<Option<Vec<u8>>>> {
    Ok(conn
        .query_row(
            "SELECT encoded_message FROM messages
             WHERE origin_namespace = ?1 AND origin_message_id = ?2",
            params![namespace, message_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// A message to store, as imported or sent to a remote address.
pub struct NewMessage<'a> {
    pub origin_namespace: &'a str,
    pub origin_message_id: &'a str,
    pub sender_address: i64,
    pub sender_run: &'a str,
    pub recipient_address: i64,
    pub recipient_run: &'a str,
    pub body: &'a str,
    pub content_type: &'a str,
    pub created_at: &'a str,
    pub authored_at: &'a str,
    pub expires_at: Option<&'a str>,
    pub in_reply_to: Option<(&'a str, &'a str)>,
    /// A group copy's original message (namespace and ID), and its original
    /// sender's address, when known here, and run.
    pub group_copy: Option<(&'a str, &'a str, Option<i64>, Option<&'a str>)>,
    pub encoded: &'a [u8],
}

/// Store a message with its exact common bytes; returns its native ID.
pub fn insert_encoded(conn: &Connection, m: &NewMessage) -> Result<i64> {
    let (reply_ns, reply_id) = m.in_reply_to.unzip();
    let (copy_ns, copy_id, copy_sender, copy_run) = match m.group_copy {
        Some((ns, id, sender, run)) => (Some(ns), Some(id), sender, run),
        None => (None, None, None, None),
    };
    conn.execute(
        "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
             sender_run_id, recipient_address_id, recipient_run_id, body, content_type,
             created_at, authored_at, expires_at, in_reply_to_namespace, in_reply_to_message_id,
             encoded_message, group_copy_namespace, group_copy_message_id,
             group_copy_sender_address_id, group_copy_sender_run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            m.origin_namespace,
            m.origin_message_id,
            m.sender_address,
            m.sender_run,
            m.recipient_address,
            m.recipient_run,
            m.body,
            m.content_type,
            m.created_at,
            m.authored_at,
            m.expires_at,
            reply_ns,
            reply_id,
            m.encoded,
            copy_ns,
            copy_id,
            copy_sender,
            copy_run,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Deliver an imported message to its native recipient's inbox.
pub fn insert_delivery(conn: &Connection, message_id: i64, address: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO deliveries (message_id, recipient_address_id, direct) VALUES (?1, ?2, 1)",
        params![message_id, address],
    )?;
    Ok(())
}

// ---- the outbox --------------------------------------------------------------

/// Record pending handoff of `message_id` through `integration`.
pub fn insert_outbox(
    conn: &Connection,
    integration: &str,
    message_id: i64,
    handoff_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO integration_outbox (integration_id, message_id, handoff_id)
         VALUES (?1, ?2, ?3)",
        params![integration, message_id, handoff_id],
    )?;
    Ok(())
}

/// The oldest pending handoff for `integration`: its outbox row, handoff ID,
/// exact message bytes and delivery deadline.
pub struct Pending {
    pub id: i64,
    pub handoff_id: String,
    pub encoded: Option<Vec<u8>>,
    pub expires_at: Option<String>,
}

pub fn next_pending(conn: &Connection, integration: &str) -> Result<Option<Pending>> {
    Ok(conn
        .query_row(
            "SELECT o.id, o.handoff_id, m.encoded_message, m.expires_at
             FROM integration_outbox o JOIN messages m ON m.id = o.message_id
             WHERE o.integration_id = ?1 AND o.state = 'pending'
             ORDER BY o.id LIMIT 1",
            [integration],
            |r| {
                Ok(Pending {
                    id: r.get(0)?,
                    handoff_id: r.get(1)?,
                    encoded: r.get(2)?,
                    expires_at: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Move outbox row `id` from `from` to `to`; returns whether it was in `from`.
pub fn set_outbox_state(
    conn: &Connection,
    id: i64,
    from: &[&str],
    to: &str,
    detail: Option<&str>,
) -> Result<bool> {
    let states = from
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(conn.execute(
        &format!(
            "UPDATE integration_outbox SET state = ?2, detail = coalesce(?3, detail)
             WHERE id = ?1 AND state IN ({states})"
        ),
        params![id, to, detail],
    )? > 0)
}

/// The outbox row and state of `integration`'s handoff `handoff_id`.
pub fn outbox_handoff(
    conn: &Connection,
    integration: &str,
    handoff_id: &str,
) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, state FROM integration_outbox
             WHERE integration_id = ?1 AND handoff_id = ?2",
            params![integration, handoff_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The state and detail of the handoff of native message `message_id`.
pub fn outbox_state(
    conn: &Connection,
    message_id: i64,
) -> Result<Option<(String, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT state, detail FROM integration_outbox WHERE message_id = ?1",
            [message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Pending handoffs whose message's deadline is at or before `now`.
pub fn expired_pending(conn: &Connection, now: &str) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(
            "SELECT o.id FROM integration_outbox o JOIN messages m ON m.id = o.message_id
             WHERE o.state = 'pending' AND m.expires_at <= ?1 ORDER BY o.id",
        )?
        .query_map([now], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// What a delivery notice for outbox row `id` needs: the message's native
/// ID, sender address, origin ID, send time and remote recipient's reference.
pub struct Failed {
    pub message: i64,
    pub sender_address: i64,
    pub origin_namespace: String,
    pub origin_message_id: String,
    pub created_at: String,
    pub recipient: String,
}

pub fn failed(conn: &Connection, id: i64) -> Result<Failed> {
    Ok(conn.query_row(
        &format!(
            "SELECT m.id, m.sender_address_id, m.origin_namespace, m.origin_message_id,
                    m.created_at, {}
             FROM integration_outbox o JOIN messages m ON m.id = o.message_id
             JOIN entity_addresses xa ON xa.id = m.recipient_address_id
             JOIN entities xe ON xe.id = xa.entity_id
             WHERE o.id = ?1",
            super::remote_display_sql("xe", "xa", "m.recipient_run_id")
        ),
        [id],
        |r| {
            Ok(Failed {
                message: r.get(0)?,
                sender_address: r.get(1)?,
                origin_namespace: r.get(2)?,
                origin_message_id: r.get(3)?,
                created_at: r.get(4)?,
                recipient: r.get(5)?,
            })
        },
    )?)
}

/// Tell the sender of a failed remote message, in its own inbox: a
/// `delivery_error` message from its own address to itself, in reply to the
/// failed message, with one delivery. Returns the notice's native ID.
pub fn insert_notice(
    conn: &Connection,
    failed: &Failed,
    message_id: &str,
    body: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
             recipient_address_id, body, created_at, authored_at, in_reply_to_namespace,
             in_reply_to_message_id, kind)
         SELECT namespace, ?1, ?2, ?2, ?3, ?4, ?4, ?5, ?6, 'delivery_error' FROM bbs_installation",
        params![
            message_id,
            failed.sender_address,
            body,
            now,
            failed.origin_namespace,
            failed.origin_message_id
        ],
    )?;
    let id = conn.last_insert_rowid();
    insert_delivery(conn, id, failed.sender_address)?;
    Ok(id)
}

/// Every attempted handoff, oldest first.
pub fn attempted(conn: &Connection) -> Result<Vec<i64>> {
    Ok(conn
        .prepare("SELECT id FROM integration_outbox WHERE state = 'attempted' ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}
