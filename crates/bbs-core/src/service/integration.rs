//! Hub's integration stream, `bbs.integration.v1`, over the database: the
//! integration credential, bounded validation of every adapter frame, the
//! publication catalog (D090), the outbound handoff outbox with
//! attempt-before-send claims (D072), and imports with duplicate and
//! conflict checks. The server owns the stream; this owns the rules.
//!
//! Rejection reasons are Hub's transfer reasons (`invalid_message`,
//! `frame_too_large`, `message_conflict`, `recipient_closed`, ...), so the
//! adapter can pass them on unchanged.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use prost::Message as _;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::{Ctx, NO_CALLER, fmt_time, with_ctx};
use crate::auth::{self, AuthenticationError};
use crate::db::{self, integration as store};
use crate::entity::{self, NATIVE_ADDRESS};
use crate::hub_proto::integration as pb;
use crate::hub_proto::msgproto_v1 as mp;

/// The integration protocol major this server speaks.
pub const PROTOCOL_MAJOR: u32 = 1;
/// The gRPC metadata carrying the integration credential.
pub const TOKEN_METADATA: &str = "bbs-integration-token";
/// The largest encoded stream frame, in either direction.
pub const MAX_FRAME_BYTES: usize = 4 << 20;
/// The largest exact common `Message` encoding.
pub const MAX_MESSAGE_BYTES: usize = 96 << 10;
/// The largest message body.
pub const MAX_BODY_BYTES: usize = 64 << 10;
/// The only content BBS sends and accepts.
pub const CONTENT_TYPE: &str = "text/plain; charset=utf-8";
/// How long a remote send may wait for its handoff (Hub's default lifetime).
pub const DELIVERY_LIFETIME: chrono::Duration = chrono::Duration::days(7);
/// The most entities one observation or directory result may carry, and the
/// largest directory page.
pub const MAX_ENTITIES: usize = 100;
const DEFAULT_PAGE: u32 = 50;
const MAX_LABEL_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 4096;
const MAX_DETAIL_BYTES: usize = 1024;
const MAX_CODE_BYTES: usize = 64;
const MAX_ALIAS_BYTES: usize = 64;
const MAX_ALIASES: usize = 32;
const MAX_ADDRESSES: usize = 8;
const MAX_DESCRIPTOR_BYTES: usize = 8 << 10;
const MAX_KIND_BYTES: usize = 64;
const MAX_PAGE_TOKEN_BYTES: usize = 256;

/// An adapter frame that breaks the protocol: the stream ends with
/// `InvalidArgument` and this message.
#[derive(Debug)]
pub struct InvalidFrame(pub String);

impl std::fmt::Display for InvalidFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidFrame {}

/// A peer this server cannot serve, such as another protocol major: the
/// stream ends with `FailedPrecondition` and this message.
#[derive(Debug)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

fn invalid(detail: impl Into<String>) -> anyhow::Error {
    InvalidFrame(detail.into()).into()
}

// ---- credentials and hello -------------------------------------------------

/// The enabled integration `token` authenticates.
pub fn authenticate(conn: &Connection, token: Option<&str>) -> Result<String> {
    let token = token.ok_or(AuthenticationError(
        "integration credential required (bbs-integration-token)",
    ))?;
    store::by_token_hash(conn, &auth::token_hash(token))?
        .ok_or_else(|| AuthenticationError("invalid or revoked integration credential").into())
}

/// Check the adapter's first frame against its credential.
pub fn check_hello(integration: &str, hello: &pb::AdapterHello) -> Result<()> {
    if hello.integration_id != integration {
        return Err(AuthenticationError(
            "the hello's integration_id does not match the integration credential",
        )
        .into());
    }
    if hello.protocol_major != PROTOCOL_MAJOR {
        return Err(Unsupported(format!(
            "unsupported_protocol: integration protocol major {} is not supported; this \
             server speaks {PROTOCOL_MAJOR}",
            hello.protocol_major
        ))
        .into());
    }
    if hello.build.len() > MAX_LABEL_BYTES {
        return Err(invalid(format!(
            "hello build is over {MAX_LABEL_BYTES} bytes"
        )));
    }
    Ok(())
}

/// BBS's hello for `integration`.
pub fn hello(conn: &Connection, integration: &str) -> Result<pb::BbsHello> {
    Ok(pb::BbsHello {
        integration_id: integration.to_string(),
        protocol_major: PROTOCOL_MAJOR,
        namespace: db::namespace(conn)?,
        build: format!("bbs-server {}", crate::release::VERSION),
    })
}

// ---- publication (D090) ----------------------------------------------------

fn address_ref(
    namespace: &str,
    entity_id: &str,
    address_id: &str,
    run: Option<&str>,
) -> mp::AddressRef {
    mp::AddressRef {
        entity: Some(mp::EntityRef {
            namespace: namespace.to_string(),
            entity_id: entity_id.to_string(),
        }),
        address_id: address_id.to_string(),
        run_id: run.map(str::to_string),
    }
}

/// Hub's alias form: 1-64 characters of `[a-z0-9._-]`, starting with a
/// letter or digit, after lowercasing. None when `alias` is not one.
pub fn normalize_alias(alias: &str) -> Option<String> {
    let a = alias.to_ascii_lowercase();
    let ok = (1..=MAX_ALIAS_BYTES).contains(&a.len())
        && a.as_bytes()[0].is_ascii_alphanumeric()
        && a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    ok.then_some(a)
}

/// A top-level entity answers to its own ID as an alias, normalized, as
/// Hub's entities do by default; a second-level one (`will/scout`) has none.
pub fn default_alias(entity_id: &str) -> Option<String> {
    if entity_id.contains('/') {
        return None;
    }
    normalize_alias(entity_id)
}

/// A published entity's descriptor: its one native address, which takes
/// plain text sends. `allowed` reports send as allowed for a directory
/// requester; the catalog is for no requester in particular.
fn descriptor(namespace: &str, p: store::Published, allowed: bool) -> mp::EntityDescriptor {
    let send = vec![mp::Operation::Send as i32];
    mp::EntityDescriptor {
        entity: Some(mp::EntityRef {
            namespace: namespace.to_string(),
            entity_id: p.entity_id.clone(),
        }),
        kind: p.kind,
        display_name: p.display_name,
        aliases: default_alias(&p.entity_id).into_iter().collect(),
        description: String::new(),
        lifecycle: mp::Lifecycle::Active as i32,
        addresses: vec![mp::AddressDescriptor {
            address: Some(address_ref(namespace, &p.entity_id, NATIVE_ADDRESS, None)),
            label: "BBS".to_string(),
            lifecycle: mp::Lifecycle::Active as i32,
            availability: mp::Availability::Unknown as i32,
            supported_operations: send.clone(),
            allowed_operations: if allowed { send } else { Vec::new() },
            content_types: vec![CONTENT_TYPE.to_string()],
            max_body_bytes: MAX_BODY_BYTES as u32,
            supports_run_selector: false,
            is_default: true,
        }],
    }
}

/// What the catalog depends on: the native entities' epoch (bumped by any
/// change to one, from any process) and `integration`'s publication switch.
/// Cheap; the stream rebuilds the catalog only when this changes. A disabled
/// (revoked) integration fails as its credential would.
pub fn catalog_key(conn: &Connection, integration: &str) -> Result<(i64, bool)> {
    let publish = store::publishes(conn, integration)?.ok_or(AuthenticationError(
        "invalid or revoked integration credential",
    ))?;
    Ok((store::catalog_epoch(conn)?, publish))
}

/// A publication snapshot, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Catalog {
    Snapshot(pb::PublishedCatalog),
    /// Over one frame: chunked catalogs are later work, so it is not sent and
    /// gets no revision.
    TooLarge {
        entities: usize,
        bytes: usize,
    },
}

/// `integration`'s current publication snapshot: every active native human
/// and agent when its switch is on, else none. A snapshot that differs from
/// the last one gets the next revision, kept in the database so it only grows
/// across restarts; a repeat keeps its revision.
pub fn catalog(conn: &mut Connection, integration: &str) -> Result<Catalog> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let (_, publish) = catalog_key(ctx.conn, integration)?;
        let namespace = db::namespace(ctx.conn)?;
        let entities: Vec<mp::EntityDescriptor> = if publish {
            store::published(ctx.conn, "", -1)?
                .into_iter()
                .map(|p| descriptor(&namespace, p, false))
                .collect()
        } else {
            Vec::new()
        };
        let mut snapshot = pb::PublishedCatalog {
            revision: 0,
            entities,
        };
        // The revision's varint adds at most ten bytes.
        let bytes = pb::BbsFrame {
            payload: Some(pb::bbs_frame::Payload::Published(snapshot.clone())),
        }
        .encoded_len()
            + 10;
        if bytes > MAX_FRAME_BYTES {
            return Ok(Catalog::TooLarge {
                entities: snapshot.entities.len(),
                bytes,
            });
        }
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(snapshot.encode_to_vec()));
        snapshot.revision = store::publication_revision(ctx.conn, integration, &digest)?;
        Ok(Catalog::Snapshot(snapshot))
    })
}

// ---- outbound handoff (D072) ---------------------------------------------------

/// What [`claim`] did.
#[derive(Debug, Default)]
pub struct Claim {
    /// The claimed outbox row and its offer, if any.
    pub handoff: Option<(i64, pb::MessageOffer)>,
    /// Rows rejected instead (expired, or without an encoding).
    pub failed: usize,
}

/// Claim `integration`'s oldest pending handoff: mark it attempted, and
/// commit, before the caller sends it. Work whose deadline passed is
/// rejected as expired instead; work held while publication is off stays
/// pending. Returns the outbox row and the offer to send, and how many rows
/// it rejected on the way (each mailed its sender a notice).
pub fn claim(conn: &mut Connection, integration: &str) -> Result<Claim> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let mut claim = Claim::default();
        if store::publishes(ctx.conn, integration)? != Some(true) {
            return Ok(claim);
        }
        while let Some(work) = store::next_pending(ctx.conn, integration)? {
            let expired = work
                .expires_at
                .as_deref()
                .map(|t| DateTime::parse_from_rfc3339(t).map(|t| t <= ctx.now))
                .transpose()?
                .unwrap_or(false);
            let Some(bytes) = work.encoded.filter(|_| !expired) else {
                let (reason, detail) = if expired {
                    (
                        "expired",
                        "its delivery deadline passed before the adapter took it",
                    )
                } else {
                    ("invalid_message", "the message has no frozen encoding")
                };
                if fail(ctx, work.id, &["pending"], reason, detail)? {
                    claim.failed += 1;
                }
                continue;
            };
            if !store::set_outbox_state(ctx.conn, work.id, &["pending"], "attempted", None)? {
                continue;
            }
            claim.handoff = Some((
                work.id,
                pb::MessageOffer {
                    handoff_id: work.handoff_id,
                    message: Some(mp::EncodedMessage {
                        format_major: 1,
                        message_bytes: bytes,
                    }),
                },
            ));
            return Ok(claim);
        }
        Ok(claim)
    })
}

/// A claimed handoff the transport took: `sent`. It never replaces a result
/// the adapter already reported, nor an `unknown`.
pub fn handed_off(conn: &Connection, outbox: i64) -> Result<()> {
    store::set_outbox_state(conn, outbox, &["attempted"], "sent", None)?;
    Ok(())
}

/// Claimed handoffs whose frames provably never left BBS (still queued in
/// memory when their stream closed) return to pending, to be offered again
/// with the same handoff ID; no notice.
pub fn requeue(conn: &Connection, outbox: &[i64]) -> Result<()> {
    for &id in outbox {
        store::set_outbox_state(conn, id, &["attempted"], "pending", None)?;
    }
    Ok(())
}

/// Settle handoffs whose deadline has passed by `now`, telling their
/// senders: pending ones are rejected as expired, and sent ones the adapter
/// never answered become unknown. The server runs this at startup and
/// periodically. Returns how many it settled.
pub fn expire(conn: &mut Connection, now: DateTime<Utc>) -> Result<usize> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let mut n = 0;
        let now = fmt_time(now);
        for id in store::expired(ctx.conn, "sent", &now)? {
            if uncertain(
                ctx,
                id,
                &["sent"],
                "no result from the adapter by the deadline",
            )? {
                n += 1;
            }
        }
        for id in store::expired(ctx.conn, "pending", &now)? {
            if fail(
                ctx,
                id,
                &["pending"],
                "expired",
                "its delivery deadline passed before the adapter took it",
            )? {
                n += 1;
            }
        }
        Ok(n)
    })
}

/// A sender's generic, safe account of a failure reason: Hub's codes, never
/// the adapter's free text.
fn failure_text(reason: &str) -> &'static str {
    match reason {
        "recipient_unknown" => "the recipient is unknown",
        "recipient_closed" => "the recipient is closed",
        "expired" => "it expired before it could be delivered",
        "sender_not_permitted" => "the recipient does not accept messages from you",
        "frame_too_large" => "it was too large",
        "unsupported_content" => "its content is not supported",
        _ => "it was refused",
    }
}

/// Move outbox row `id` from one of `from` to `rejected`, with `reason` and
/// `detail` recorded, and tell the sender: a delivery notice in its own
/// inbox. Returns whether the row moved.
fn fail(ctx: &Ctx, id: i64, from: &[&str], reason: &str, detail: &str) -> Result<bool> {
    let recorded = if detail.is_empty() {
        reason.to_string()
    } else {
        format!("{reason}: {detail}")
    };
    if !store::set_outbox_state(ctx.conn, id, from, "rejected", Some(&recorded))? {
        return Ok(false);
    }
    let failed = store::failed(ctx.conn, id)?;
    let body = format!(
        "Not delivered: your message to {} (sent {}) was not delivered: {}.",
        failed.recipient,
        failed.created_at,
        failure_text(reason)
    );
    store::insert_notice(ctx.conn, &failed, &db::new_message_id(), &body, &ctx.now_s)?;
    tracing::info!(
        message = failed.message,
        to = failed.recipient,
        reason,
        "remote send failed"
    );
    Ok(true)
}

/// Move outbox row `id` from one of `from` to `unknown` with `detail`, and
/// tell the sender the delivery is uncertain: it may or may not have reached
/// the recipient, and BBS never sends it again. Returns whether the row
/// moved.
fn uncertain(ctx: &Ctx, id: i64, from: &[&str], detail: &str) -> Result<bool> {
    if !store::set_outbox_state(ctx.conn, id, from, "unknown", Some(detail))? {
        return Ok(false);
    }
    let failed = store::failed(ctx.conn, id)?;
    let body = format!(
        "Delivery uncertain: your message to {} (sent {}) may not have arrived. BBS will \
         not send it again.",
        failed.recipient, failed.created_at
    );
    store::insert_notice(ctx.conn, &failed, &db::new_message_id(), &body, &ctx.now_s)?;
    tracing::info!(
        message = failed.message,
        to = failed.recipient,
        detail,
        "remote send uncertain"
    );
    Ok(true)
}

/// Handoffs a stopped server left attempted may or may not have reached the
/// adapter: they become unknown, with a notice to each sender. The server
/// runs this once at startup. Returns how many.
pub fn interrupt_attempted(conn: &mut Connection) -> Result<usize> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        let mut n = 0;
        for id in store::attempted(ctx.conn)? {
            if uncertain(
                ctx,
                id,
                &["attempted"],
                "interrupted: the server stopped during the handoff",
            )? {
                n += 1;
            }
        }
        Ok(n)
    })
}

fn check_text(what: &str, text: &str, max: usize) -> Result<()> {
    if text.len() > max {
        return Err(invalid(format!("{what} is over {max} bytes")));
    }
    Ok(())
}

/// Record the adapter's informative result for one of `integration`'s
/// handoffs: received (or already known), or rejected, which sends the sender
/// a delivery notice. A result for a handoff never offered is ignored, and
/// the first result stands. Returns whether the handoff's state changed.
pub fn record_result(
    conn: &mut Connection,
    integration: &str,
    result: &pb::HandoffResult,
) -> Result<bool> {
    if !entity::valid_id(&result.handoff_id) {
        return Err(invalid("outgoing_result has an invalid handoff_id"));
    }
    check_text("outgoing_result reason", &result.reason, MAX_CODE_BYTES)?;
    check_text("outgoing_result detail", &result.detail, MAX_DETAIL_BYTES)?;
    let (state, detail) = match pb::HandoffDisposition::try_from(result.disposition) {
        Ok(pb::HandoffDisposition::Received) => ("received", None),
        Ok(pb::HandoffDisposition::AlreadyKnown) => ("received", Some("already_known")),
        Ok(pb::HandoffDisposition::Rejected) => ("rejected", None),
        _ => return Err(invalid("outgoing_result has no disposition")),
    };
    with_ctx(conn, &NO_CALLER, |ctx| {
        let Some((id, _)) = store::outbox_handoff(ctx.conn, integration, &result.handoff_id)?
        else {
            tracing::warn!(
                integration,
                handoff = result.handoff_id,
                "result for an unknown handoff"
            );
            return Ok(false);
        };
        let from = &["attempted", "sent", "unknown"];
        if state == "rejected" {
            let reason = if result.reason.is_empty() {
                "rejected"
            } else {
                &result.reason
            };
            return fail(ctx, id, from, reason, &result.detail);
        }
        store::set_outbox_state(ctx.conn, id, from, state, detail)
    })
}

/// Send a native message to a remote address: the message and its pending
/// handoff are stored together, in the caller's transaction. Publication must
/// be on for the integration that reaches the address.
pub(super) fn send_remote(
    ctx: &Ctx,
    me: &db::Principal,
    reference: &str,
    body: &str,
) -> Result<(i64, String)> {
    let remote = entity::parse_remote(reference)?;
    let (namespace, entity_id, address_id) = (
        remote.address.entity.namespace.as_str(),
        remote.address.entity.entity_id.as_str(),
        remote.address.address_id.as_str(),
    );
    let local = db::namespace(ctx.conn)?;
    if namespace == local {
        bail!("id:{reference} is in this installation's namespace ({local}); send to its name");
    }
    // Every refusal comes before any row is written.
    let usable = store::usable(ctx.conn)?;
    let integration = match store::namespace_integration(ctx.conn, namespace)? {
        Some(bound) => bound,
        None => match usable.as_slice() {
            [] => bail!(
                "remote sends are off: no integration is enabled; an administrator adds \
                 one with `bbs admin integration add`"
            ),
            [(id, _)] => id.clone(),
            _ => bail!(
                "several integrations are enabled and none reaches namespace {namespace} \
                 yet; BBS cannot choose one"
            ),
        },
    };
    match usable.iter().find(|(id, _)| *id == integration) {
        Some((_, true)) => {}
        Some((_, false)) => bail!(
            "remote sends are off: integration {integration} does not publish this \
             installation's entities; an administrator turns that on with `bbs admin \
             integration publish {integration} on`"
        ),
        None => bail!(
            "remote sends are off: integration {integration}, which reaches namespace \
             {namespace}, is disabled or has no credential"
        ),
    }
    if store::foreign_retired(ctx.conn, namespace, entity_id)? {
        bail!("id:{reference} is retired, as the integration last observed it");
    }
    super::check_focus(ctx, me, None)?;
    let sender = db::native_address(ctx.conn, me.kind.into(), me.id)?
        .context("the sender has no native address")?;
    store::bind_namespace(ctx.conn, namespace, &integration)?;
    let entity = store::foreign_entity(ctx.conn, namespace, entity_id, "unknown", &ctx.now_s)?;
    let recipient = store::foreign_address(ctx.conn, entity, address_id, &integration)?;
    let message_id = db::new_message_id();
    let expires = ctx.now + DELIVERY_LIFETIME;
    let run = remote.run.as_deref();
    let message = mp::Message {
        id: Some(mp::MessageRef {
            namespace: local.clone(),
            message_id: message_id.clone(),
        }),
        sender: Some(address_ref(
            &sender.entity.namespace,
            &sender.entity.entity_id,
            &sender.address_id,
            None,
        )),
        recipient: Some(address_ref(namespace, entity_id, address_id, run)),
        authored_at_unix_micros: ctx.now.timestamp_micros(),
        expires_at_unix_micros: Some(expires.timestamp_micros()),
        content_type: CONTENT_TYPE.to_string(),
        body: body.as_bytes().to_vec(),
        in_reply_to: None,
        kind: mp::MessageKind::Application as i32,
        delivery_error: None,
        group_copy: None,
    };
    // Frozen once: the adapter signs exactly these bytes.
    let encoded = message.encode_to_vec();
    let id = store::insert_encoded(
        ctx.conn,
        &store::NewMessage {
            origin_namespace: &local,
            origin_message_id: &message_id,
            sender_address: db::native_address_row(ctx.conn, me.kind.into(), me.id)?,
            sender_run: "",
            recipient_address: recipient,
            recipient_run: run.unwrap_or(""),
            body,
            content_type: CONTENT_TYPE,
            created_at: &ctx.now_s,
            authored_at: &ctx.now_s,
            expires_at: Some(&fmt_time(expires)),
            in_reply_to: None,
            group_copy: None,
            encoded: &encoded,
        },
    )?;
    store::insert_outbox(ctx.conn, &integration, id, &db::new_message_id())?;
    let to = entity::remote_display(namespace, entity_id, address_id, run.unwrap_or(""));
    tracing::info!(
        from = me.name,
        to,
        integration,
        bytes = body.len(),
        "queued remote send"
    );
    Ok((id, to))
}

// ---- inbound import ------------------------------------------------------------

/// A refusal of an incoming offer, with Hub's reason.
struct Refusal {
    reason: &'static str,
    detail: String,
}

fn refuse(reason: &'static str, detail: impl Into<String>) -> Refusal {
    Refusal {
        reason,
        detail: detail.into(),
    }
}

fn valid_address(a: Option<&mp::AddressRef>) -> bool {
    a.is_some_and(|a| {
        a.entity.as_ref().is_some_and(|e| {
            db::validate_namespace(&e.namespace).is_ok() && entity::valid_entity_id(&e.entity_id)
        }) && entity::valid_id(&a.address_id)
            && a.run_id.as_deref().is_none_or(entity::valid_id)
    })
}

fn valid_ref(r: Option<&mp::MessageRef>) -> bool {
    r.is_some_and(|r| {
        db::validate_namespace(&r.namespace).is_ok() && entity::valid_id(&r.message_id)
    })
}

/// The structural rules every hop applies (Hub's `checkMessage`), plus
/// D126's two-level entity IDs, group-copy shape and a sender in the
/// message's own namespace.
fn check_message(m: &mp::Message) -> Result<(), Refusal> {
    if !valid_ref(m.id.as_ref()) {
        return Err(refuse("invalid_message", "message ID is incomplete"));
    }
    if !valid_address(m.sender.as_ref()) || !valid_address(m.recipient.as_ref()) {
        return Err(refuse(
            "invalid_message",
            "sender and recipient must be complete addresses",
        ));
    }
    if m.kind != mp::MessageKind::Application as i32 || m.delivery_error.is_some() {
        return Err(refuse(
            "unsupported_content",
            "only APPLICATION messages are accepted",
        ));
    }
    if m.authored_at_unix_micros <= 0 || m.expires_at_unix_micros.is_none() {
        return Err(refuse(
            "invalid_message",
            "authored time and delivery deadline are required",
        ));
    }
    if m.body.len() > MAX_BODY_BYTES {
        return Err(refuse(
            "frame_too_large",
            format!("body exceeds {MAX_BODY_BYTES} bytes"),
        ));
    }
    if m.content_type.is_empty() || m.content_type.len() > 128 {
        return Err(refuse("invalid_message", "invalid content type"));
    }
    if m.in_reply_to.is_some() && !valid_ref(m.in_reply_to.as_ref()) {
        return Err(refuse("invalid_message", "invalid in_reply_to"));
    }
    if let Some(copy) = &m.group_copy {
        if !valid_ref(copy.original.as_ref()) || !valid_address(copy.original_sender.as_ref()) {
            return Err(refuse(
                "invalid_message",
                "a group copy needs its original message and original sender",
            ));
        }
        if m.in_reply_to.is_some() {
            return Err(refuse(
                "invalid_message",
                "a group copy has no in_reply_to; replies use group_copy",
            ));
        }
        let original = copy.original.as_ref().unwrap();
        let by = copy.original_sender.as_ref().unwrap();
        if by.entity.as_ref().unwrap().namespace != original.namespace {
            return Err(refuse(
                "invalid_message",
                "a group copy's original sender must be in its original message's namespace",
            ));
        }
    }
    let (id, sender) = (m.id.as_ref().unwrap(), m.sender.as_ref().unwrap());
    if sender.entity.as_ref().unwrap().namespace != id.namespace {
        return Err(refuse(
            "invalid_message",
            "the sender must be in the message's own namespace",
        ));
    }
    Ok(())
}

fn micros_time(micros: i64) -> Result<DateTime<Utc>, Refusal> {
    DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| refuse("invalid_message", "timestamp out of range"))
}

/// Import one incoming offer from `integration`: validate it, then report
/// a matching duplicate, refuse a conflict, or store the message with its
/// native delivery in one transaction. Every outcome is a result for the
/// adapter; a storage failure is `storage_unavailable`.
pub fn import(
    conn: &mut Connection,
    integration: &str,
    offer: &pb::MessageOffer,
) -> pb::HandoffResult {
    let mut result = pb::HandoffResult {
        handoff_id: offer.handoff_id.clone(),
        ..Default::default()
    };
    let outcome = with_ctx(conn, &NO_CALLER, |ctx| import_in(ctx, integration, offer));
    match outcome {
        Ok(Ok(disposition)) => result.disposition = disposition as i32,
        Ok(Err(refusal)) => {
            result.disposition = pb::HandoffDisposition::Rejected as i32;
            result.reason = refusal.reason.to_string();
            result.detail = refusal.detail;
        }
        Err(err) => {
            tracing::error!(integration, "importing a message: {err:#}");
            result.disposition = pb::HandoffDisposition::Rejected as i32;
            result.reason = "storage_unavailable".to_string();
            result.detail = "BBS could not store the message".to_string();
        }
    }
    result
}

fn import_in(
    ctx: &Ctx,
    integration: &str,
    offer: &pb::MessageOffer,
) -> Result<Result<pb::HandoffDisposition, Refusal>> {
    macro_rules! refused {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(refusal) => return Ok(Err(refusal)),
            }
        };
    }
    if !entity::valid_id(&offer.handoff_id) {
        return Ok(Err(refuse("invalid_message", "invalid handoff ID")));
    }
    let Some(encoded) = &offer.message else {
        return Ok(Err(refuse("invalid_message", "the offer has no message")));
    };
    if encoded.format_major != 1 {
        return Ok(Err(refuse(
            "invalid_message",
            format!("unsupported message format_major {}", encoded.format_major),
        )));
    }
    let bytes = &encoded.message_bytes;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Ok(Err(refuse(
            "frame_too_large",
            format!("{} bytes; limit {MAX_MESSAGE_BYTES}", bytes.len()),
        )));
    }
    let m = refused!(
        mp::Message::decode(bytes.as_slice())
            .map_err(|err| refuse("invalid_message", format!("invalid protobuf: {err}")))
    );
    refused!(check_message(&m));
    let id = m.id.as_ref().unwrap();
    let local = db::namespace(ctx.conn)?;
    if id.namespace == local {
        return Ok(Err(refuse(
            "route_not_authorized",
            format!("messages from {local} originate only at its home"),
        )));
    }
    if m.group_copy
        .as_ref()
        .is_some_and(|c| c.original.as_ref().unwrap().namespace == local)
    {
        return Ok(Err(refuse(
            "invalid_message",
            format!("a group copy cannot name one of {local}'s entities as its original sender"),
        )));
    }
    // A namespace is reached through one integration (see `send_remote`), so
    // the duplicate check below is that integration's. A group copy's
    // original speaks for its namespace too.
    let original = m
        .group_copy
        .as_ref()
        .map(|c| &c.original.as_ref().unwrap().namespace);
    for namespace in std::iter::once(&id.namespace).chain(original) {
        if let Some(bound) = store::namespace_integration(ctx.conn, namespace)?
            && bound != integration
        {
            return Ok(Err(refuse(
                "route_not_authorized",
                format!("namespace {namespace} is reached through another integration"),
            )));
        }
    }
    // Hub's order: a duplicate is answered before any lifecycle or deadline
    // check, so an identical retry reports the original acceptance.
    if let Some(stored) = store::origin_bytes(ctx.conn, &id.namespace, &id.message_id)? {
        return Ok(if stored.as_deref() == Some(bytes.as_slice()) {
            Ok(pb::HandoffDisposition::AlreadyKnown)
        } else {
            Err(refuse(
                "message_conflict",
                format!(
                    "{}:{} is already known with different content",
                    id.namespace, id.message_id
                ),
            ))
        });
    }
    let expires = refused!(micros_time(m.expires_at_unix_micros.unwrap_or_default()));
    if expires <= ctx.now {
        return Ok(Err(refuse("expired", "the delivery deadline has passed")));
    }
    let authored = refused!(micros_time(m.authored_at_unix_micros));
    if m.content_type != CONTENT_TYPE {
        return Ok(Err(refuse(
            "unsupported_content",
            format!("BBS accepts only {CONTENT_TYPE}"),
        )));
    }
    let Ok(body) = std::str::from_utf8(&m.body) else {
        return Ok(Err(refuse("unsupported_content", "the body is not UTF-8")));
    };
    if body.trim().is_empty() {
        return Ok(Err(refuse("invalid_message", "the body is empty")));
    }

    // Whether the recipient is unpublished, absent, a group or retired is
    // never told apart: each is `recipient_unknown`, with one detail.
    let unknown = || Ok(Err(refuse("recipient_unknown", "no such recipient")));
    let recipient = m.recipient.as_ref().unwrap();
    let to = recipient.entity.as_ref().unwrap();
    if to.namespace != local
        || recipient.run_id.is_some()
        || store::publishes(ctx.conn, integration)? != Some(true)
    {
        return unknown();
    }
    let Some(target) =
        store::native_target(ctx.conn, &local, &to.entity_id, &recipient.address_id)?
    else {
        return unknown();
    };
    if target.group_id.is_some() || !target.active {
        return unknown();
    }
    if let Some(agent) = target.agent_id
        && db::focused_at(ctx.conn, agent)?.is_some()
    {
        return Ok(Err(refuse(
            "sender_not_permitted",
            "the recipient is not accepting messages",
        )));
    }

    let sender = m.sender.as_ref().unwrap();
    let from = sender.entity.as_ref().unwrap();
    // A group copy's sender is the group; nothing else says what a sender is.
    let kind = if m.group_copy.is_some() {
        "group"
    } else {
        "unknown"
    };
    store::bind_namespace(ctx.conn, &id.namespace, integration)?;
    let entity =
        store::foreign_entity(ctx.conn, &from.namespace, &from.entity_id, kind, &ctx.now_s)?;
    let sender_address = store::foreign_address(ctx.conn, entity, &sender.address_id, integration)?;
    let copy = match &m.group_copy {
        Some(copy) => {
            let original = copy.original.as_ref().unwrap();
            let by = copy.original_sender.as_ref().unwrap();
            let by_entity = by.entity.as_ref().unwrap();
            // Another installation's entity (checked above), possibly unknown
            // here until now.
            let e = store::foreign_entity(
                ctx.conn,
                &by_entity.namespace,
                &by_entity.entity_id,
                "unknown",
                &ctx.now_s,
            )?;
            let address = Some(store::foreign_address(
                ctx.conn,
                e,
                &by.address_id,
                integration,
            )?);
            Some((
                original.namespace.as_str(),
                original.message_id.as_str(),
                address,
                by.run_id.as_deref(),
            ))
        }
        None => None,
    };
    let authored_s = fmt_time(authored);
    let expires_s = fmt_time(expires);
    let message = store::insert_encoded(
        ctx.conn,
        &store::NewMessage {
            origin_namespace: &id.namespace,
            origin_message_id: &id.message_id,
            sender_address,
            sender_run: sender.run_id.as_deref().unwrap_or(""),
            recipient_address: target.address,
            recipient_run: "",
            body,
            content_type: &m.content_type,
            created_at: &ctx.now_s,
            authored_at: &authored_s,
            expires_at: Some(&expires_s),
            in_reply_to: m
                .in_reply_to
                .as_ref()
                .map(|r| (r.namespace.as_str(), r.message_id.as_str())),
            group_copy: copy,
            encoded: bytes,
        },
    )?;
    store::insert_delivery(ctx.conn, message, target.address)?;
    tracing::info!(
        integration,
        message,
        origin = format!("{}:{}", id.namespace, id.message_id),
        "imported message"
    );
    Ok(Ok(pb::HandoffDisposition::Received))
}

// ---- observations and directory ------------------------------------------------

fn check_descriptor(d: &mp::EntityDescriptor) -> Result<()> {
    let ok = d.entity.as_ref().is_some_and(|e| {
        db::validate_namespace(&e.namespace).is_ok() && entity::valid_entity_id(&e.entity_id)
    });
    if !ok {
        return Err(invalid("an entity descriptor has an invalid entity"));
    }
    let kind_ok = !d.kind.is_empty()
        && d.kind.len() <= MAX_KIND_BYTES
        && d.kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !kind_ok {
        return Err(invalid("an entity descriptor has an invalid kind"));
    }
    check_text("an entity's display name", &d.display_name, MAX_LABEL_BYTES)?;
    check_text(
        "an entity's description",
        &d.description,
        MAX_DESCRIPTION_BYTES,
    )?;
    if d.aliases.len() > MAX_ALIASES || d.aliases.iter().any(|a| a.len() > MAX_ALIAS_BYTES) {
        return Err(invalid(
            "an entity descriptor has too many or too long aliases",
        ));
    }
    if d.addresses.len() > MAX_ADDRESSES {
        return Err(invalid("an entity descriptor has too many addresses"));
    }
    if d.encoded_len() > MAX_DESCRIPTOR_BYTES {
        return Err(invalid(format!(
            "an entity descriptor is over {MAX_DESCRIPTOR_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Record what the adapter observed about foreign entities: kind, display
/// name and lifecycle. Only descriptors in a namespace bound to
/// `integration` count; the rest, this installation's own included, are
/// ignored, so observations never change native identities or another
/// integration's.
pub fn observe(
    conn: &mut Connection,
    integration: &str,
    observed: &pb::ObservedEntities,
) -> Result<()> {
    if observed.entities.len() > MAX_ENTITIES {
        return Err(invalid(format!(
            "observed carries over {MAX_ENTITIES} entities"
        )));
    }
    for d in &observed.entities {
        check_descriptor(d)?;
    }
    with_ctx(conn, &NO_CALLER, |ctx| {
        let local = db::namespace(ctx.conn)?;
        for d in &observed.entities {
            let e = d.entity.as_ref().unwrap();
            // Only the integration a namespace is bound to speaks for it.
            if e.namespace == local
                || store::namespace_integration(ctx.conn, &e.namespace)?.as_deref()
                    != Some(integration)
            {
                continue;
            }
            store::observe(
                ctx.conn,
                &e.namespace,
                &e.entity_id,
                &d.kind,
                &d.display_name,
                d.lifecycle == mp::Lifecycle::Retired as i32,
                &ctx.now_s,
            )?;
        }
        Ok(())
    })
}

/// Check a directory result the adapter sent. BBS sends no remote directory
/// queries yet, so a valid one is ignored.
pub fn check_remote_directory(result: &mp::DirectoryResult) -> Result<()> {
    if !entity::valid_id(&result.query_id) {
        return Err(invalid("remote_directory_result has an invalid query_id"));
    }
    if result.entities.len() > MAX_ENTITIES || result.runs.len() > MAX_ENTITIES {
        return Err(invalid(format!(
            "remote_directory_result carries over {MAX_ENTITIES} entries"
        )));
    }
    Ok(())
}

/// Answer the adapter's directory query about this installation, for an
/// external requester the adapter vouches for. Only published entities are
/// visible: none while publication is off. A top-level entity answers to
/// its own ID as an alias (see [`default_alias`]); BBS has no other aliases,
/// and no runs.
pub fn local_directory(
    conn: &mut Connection,
    integration: &str,
    q: &mp::DirectoryQuery,
) -> Result<mp::DirectoryResult> {
    if !entity::valid_id(&q.query_id) {
        return Err(invalid("local_directory_query has an invalid query_id"));
    }
    let mut res = mp::DirectoryResult {
        query_id: q.query_id.clone(),
        ..Default::default()
    };
    let fail = |code: &str, detail: &str| mp::DirectoryResult {
        query_id: q.query_id.clone(),
        error_code: code.to_string(),
        detail: detail.to_string(),
        ..Default::default()
    };
    with_ctx(conn, &NO_CALLER, |ctx| {
        let local = db::namespace(ctx.conn)?;
        if q.namespace != local {
            return Ok(fail(
                "namespace_unknown",
                "this installation's namespace differs",
            ));
        }
        if !q.alias.is_empty() && !q.entity_id.is_empty() {
            return Ok(fail(
                "invalid_argument",
                "alias and entity_id are exclusive",
            ));
        }
        if q.list_runs && q.entity_id.is_empty() {
            return Ok(fail("invalid_argument", "list_runs requires entity_id"));
        }
        let alias = if q.alias.is_empty() {
            None
        } else {
            match normalize_alias(&q.alias) {
                Some(a) => Some(a),
                None => return Ok(fail("invalid_argument", "invalid alias")),
            }
        };
        if !q.entity_id.is_empty() && !entity::valid_entity_id(&q.entity_id) {
            return Ok(fail("invalid_argument", "invalid entity ID"));
        }
        if q.page_token.len() > MAX_PAGE_TOKEN_BYTES {
            return Ok(fail("invalid_cursor", "page token is too long"));
        }
        if store::publishes(ctx.conn, integration)? != Some(true) {
            return Ok(res);
        }
        if let Some(alias) = alias {
            if let Some(p) = store::published_by_alias(ctx.conn, &alias)? {
                res.entities.push(descriptor(&local, p, true));
            }
            return Ok(res);
        }
        if !q.entity_id.is_empty() {
            // A found entity, or with list_runs its runs: BBS has none.
            if let Some(p) = store::published_entity(ctx.conn, &q.entity_id)?
                && !q.list_runs
            {
                res.entities.push(descriptor(&local, p, true));
            }
            return Ok(res);
        }
        let after = match std::str::from_utf8(&q.page_token) {
            Ok(after) if after.is_empty() || entity::valid_entity_id(after) => after,
            _ => return Ok(fail("invalid_cursor", "invalid page token")),
        };
        let limit = match q.page_size {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_ENTITIES as u32),
        } as usize;
        let mut page = store::published(ctx.conn, after, limit as i64 + 1)?;
        if page.len() > limit {
            page.truncate(limit);
            res.next_page_token = page[limit - 1].entity_id.clone().into_bytes();
        }
        res.entities = page
            .into_iter()
            .map(|p| descriptor(&local, p, true))
            .collect();
        Ok(res)
    })
}

// ---- administration ----------------------------------------------------------

fn validate_integration_id(id: &str) -> Result<()> {
    if !entity::valid_id(id) || id.len() > 64 {
        bail!("invalid integration ID {id:?}: use 1-64 letters, digits, '_' or '-'");
    }
    Ok(())
}

fn require_integration(ctx: &Ctx, id: &str) -> Result<()> {
    if !store::exists(ctx.conn, id)? {
        bail!("no integration {id:?}; add it with `bbs admin integration add {id}`");
    }
    Ok(())
}

/// Create an integration, publication off, and issue its credential.
pub fn admin_add(conn: &mut Connection, id: &str) -> Result<String> {
    validate_integration_id(id)?;
    with_ctx(conn, &NO_CALLER, |ctx| {
        if store::exists(ctx.conn, id)? {
            bail!("integration {id:?} already exists; rotate its credential instead");
        }
        store::insert(ctx.conn, id)?;
        let token = auth::new_token();
        store::set_token(ctx.conn, id, &auth::token_hash(&token))?;
        Ok(token)
    })
}

/// Replace an integration's credential, enabling it; open streams on the old
/// one end.
pub fn admin_rotate(conn: &mut Connection, id: &str) -> Result<String> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        require_integration(ctx, id)?;
        let token = auth::new_token();
        store::set_token(ctx.conn, id, &auth::token_hash(&token))?;
        Ok(token)
    })
}

/// Clear another installation's `namespace` binding, so the next integration
/// to import from it or send to it takes it; for moving a namespace to an
/// integration that replaces its old one. Work already queued stays with the
/// integration it was queued for. Returns the integration it was bound to.
pub fn admin_unbind(conn: &mut Connection, namespace: &str) -> Result<Option<String>> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        store::unbind_namespace(ctx.conn, namespace)
    })
}

/// Revoke an integration's credential and disable it until a rotate; open
/// streams end, and remote sends through it are refused.
pub fn admin_revoke(conn: &mut Connection, id: &str) -> Result<bool> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        require_integration(ctx, id)?;
        store::revoke_token(ctx.conn, id)
    })
}

/// Turn publication (D090) on or off; returns whether it changed.
pub fn admin_publish(conn: &mut Connection, id: &str, on: bool) -> Result<bool> {
    with_ctx(conn, &NO_CALLER, |ctx| {
        require_integration(ctx, id)?;
        store::set_publish(ctx.conn, id, on)
    })
}

pub fn admin_list(conn: &mut Connection) -> Result<Vec<store::IntegrationRow>> {
    with_ctx(conn, &NO_CALLER, |ctx| store::list(ctx.conn))
}
