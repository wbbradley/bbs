//! The integration stream to BBS (`bbs.integration.v1`, D085): the adapter
//! connects with its integration credential, exchanges hellos, and then
//!
//! - keeps BBS's publication catalog (its revision only grows);
//! - takes BBS's outbound handoffs, checks them, and stores them for the
//!   relay; the relay's result goes back to BBS as the handoff's result;
//! - offers verified messages from Hub to BBS as imports, one at a time,
//!   claimed before they are queued; BBS's result goes back to the relay;
//! - asks BBS to answer Hub's directory queries for this namespace.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bbs_core::hub_proto::integration as pb;
use bbs_core::hub_proto::integration::integration_client::IntegrationClient;
use bbs_core::hub_proto::msgproto_v1 as mp;
use pb::adapter_frame::Payload as Out;
use pb::bbs_frame::Payload as In;
use tonic::Request;
use tonic::transport::Endpoint;

use crate::adapter::{Shared, bounded, code};
use crate::link::{self, Item};
use crate::store::{Direction, Intake};
use crate::verify::{self, Reason, reason};
use crate::{new_id, now_micros, wire};

/// The integration protocol major this adapter speaks.
pub const PROTOCOL_MAJOR: u32 = 1;
/// The gRPC metadata carrying the integration credential.
pub const TOKEN_METADATA: &str = "bbs-integration-token";
/// Frames queued for BBS before senders wait.
const QUEUE: usize = 16;

/// A BBS this adapter cannot serve: retrying cannot help.
#[derive(Debug)]
pub struct Incompatible(pub String);

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Incompatible {}

fn frame(payload: Out) -> pb::AdapterFrame {
    pb::AdapterFrame {
        payload: Some(payload),
    }
}

/// Keep a stream to BBS, reconnecting with backoff. Returns only for a BBS
/// this adapter cannot serve.
pub(crate) async fn run(shared: Arc<Shared>) -> Result<()> {
    let mut backoff = shared.opts.backoff_min;
    loop {
        let start = Instant::now();
        match session(&shared).await {
            Ok(()) => tracing::info!("BBS stream ended"),
            Err(err) if err.downcast_ref::<Incompatible>().is_some() => return Err(err),
            Err(err) => tracing::warn!(bbs = shared.s.bbs_endpoint, "BBS stream: {err:#}"),
        }
        if start.elapsed() > Duration::from_secs(60) {
            backoff = shared.opts.backoff_min;
        }
        let half = backoff / 2;
        let jitter = Duration::from_micros(rand::random_range(0..=half.as_micros() as u64));
        tokio::time::sleep(half + jitter).await;
        backoff = (backoff * 2).min(shared.opts.backoff_max);
    }
}

async fn session(shared: &Arc<Shared>) -> Result<()> {
    let channel = Endpoint::from_shared(shared.s.bbs_endpoint.clone())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .connect()
        .await
        .context("connecting")?;
    let (tx, frames) = link::channel::<pb::AdapterFrame>(QUEUE, shared.settle());
    tx.try_send(frame(Out::Hello(pb::AdapterHello {
        integration_id: shared.s.integration_id.clone(),
        protocol_major: PROTOCOL_MAJOR,
        build: crate::build(),
    })));
    let mut client = IntegrationClient::new(channel)
        .max_decoding_message_size(wire::MAX_FRAME)
        .max_encoding_message_size(wire::MAX_FRAME);
    let mut req = Request::new(frames);
    req.metadata_mut().insert(
        TOKEN_METADATA,
        shared
            .s
            .token
            .parse()
            .map_err(|_| anyhow::anyhow!("the integration credential is not valid metadata"))?,
    );
    let result = async {
        let mut inbound = client
            .connect(req)
            .await
            .context("opening the integration stream")?
            .into_inner();
        let hello = tokio::time::timeout(shared.opts.reply_timeout, inbound.message())
            .await
            .context("no hello from BBS")??
            .context("BBS ended the stream before its hello")?;
        let Some(In::Hello(hello)) = hello.payload else {
            bail!("BBS's first frame was not a hello");
        };
        check_hello(shared, &hello)?;
        tracing::info!(
            namespace = hello.namespace,
            build = hello.build,
            "BBS connected"
        );
        *shared.bbs.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx.clone());
        shared.report_outbound();
        shared.bbs_kick.notify_one();
        tokio::select! {
            r = read(shared, &tx, inbound) => r,
            r = dispatch(shared, &tx) => r,
        }
    }
    .await;
    {
        let mut bbs = shared.bbs.lock().unwrap_or_else(PoisonError::into_inner);
        if bbs.as_ref().is_some_and(|b| b.same(&tx)) {
            *bbs = None;
        }
    }
    tx.close();
    result
}

fn check_hello(shared: &Shared, hello: &pb::BbsHello) -> Result<()> {
    if hello.protocol_major != PROTOCOL_MAJOR {
        return Err(Incompatible(format!(
            "BBS speaks integration protocol {}; this adapter speaks {PROTOCOL_MAJOR}",
            hello.protocol_major
        ))
        .into());
    }
    if hello.integration_id != shared.s.integration_id {
        bail!(
            "BBS's hello names integration {:?}, not {:?}",
            hello.integration_id,
            shared.s.integration_id
        );
    }
    if hello.namespace != shared.s.namespace {
        return Err(Incompatible(format!(
            "BBS is namespace {:?}, but this adapter's installation credential is for {:?}",
            hello.namespace, shared.s.namespace
        ))
        .into());
    }
    Ok(())
}

async fn read(
    shared: &Arc<Shared>,
    tx: &link::Sender<pb::AdapterFrame>,
    mut inbound: tonic::Streaming<pb::BbsFrame>,
) -> Result<()> {
    while let Some(f) = inbound.message().await? {
        let reply = match f.payload {
            Some(In::Hello(_)) => bail!("BBS repeated its hello"),
            Some(In::Published(catalog)) => {
                on_catalog(shared, &catalog);
                None
            }
            Some(In::Outgoing(offer)) => {
                let result = on_outgoing(shared, &offer);
                shared.relay_kick.notify_one();
                result.map(Out::OutgoingResult)
            }
            Some(In::IncomingResult(result)) => {
                on_incoming_result(shared, &result);
                None
            }
            Some(In::LocalDirectoryResult(result)) => {
                shared.directory_answered(result);
                None
            }
            // BBS asks no remote directory queries yet; answered plainly so
            // a future BBS does not wait.
            Some(In::RemoteDirectoryQuery(q)) => {
                Some(Out::RemoteDirectoryResult(mp::DirectoryResult {
                    query_id: q.query_id,
                    error_code: "unimplemented".to_string(),
                    detail: "this adapter does not resolve remote directories yet".to_string(),
                    ..Default::default()
                }))
            }
            Some(In::Ping(p)) => Some(Out::Pong(pb::Pong { nonce: p.nonce })),
            Some(In::Pong(_)) => None,
            None => {
                tracing::debug!("ignoring a frame from BBS with no payload this adapter knows");
                None
            }
        };
        if let Some(reply) = reply {
            let Some(permit) = tx.reserve().await else {
                return Ok(());
            };
            if permit
                .send(Item {
                    frame: frame(reply),
                    claim: None,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Keep a newer publication catalog. Its entities must all be in this
/// namespace; one that is not is ignored whole.
fn on_catalog(shared: &Shared, catalog: &pb::PublishedCatalog) {
    let foreign = catalog.entities.iter().find(|d| {
        d.entity
            .as_ref()
            .is_none_or(|e| e.namespace != shared.s.namespace)
    });
    if foreign.is_some() {
        tracing::error!(
            revision = catalog.revision,
            "ignoring a catalog naming an entity outside this namespace"
        );
        return;
    }
    match shared.store().put_catalog(catalog, now_micros()) {
        Ok(true) => tracing::info!(
            revision = catalog.revision,
            entities = catalog.entities.len(),
            "BBS published its catalog"
        ),
        Ok(false) => {}
        Err(err) => tracing::error!("storing the catalog: {err:#}"),
    }
}

fn rejected(handoff_id: &str, r: Reason) -> pb::HandoffResult {
    tracing::info!(
        handoff = handoff_id,
        reason = r.code,
        "refused a remote send: {}",
        r.detail
    );
    pb::HandoffResult {
        handoff_id: handoff_id.to_string(),
        disposition: pb::HandoffDisposition::Rejected as i32,
        reason: r.code.to_string(),
        detail: bounded(&r.detail, 1024),
    }
}

/// Check one outbound handoff from BBS and store it for the relay. Returns
/// a result now only when it is refused or already known; otherwise the
/// relay's result follows.
fn on_outgoing(shared: &Shared, offer: &pb::MessageOffer) -> Option<pb::HandoffResult> {
    let id = &offer.handoff_id;
    if !verify::valid_id(id) {
        return Some(rejected(
            id,
            reason(verify::INVALID_MESSAGE, "invalid handoff ID"),
        ));
    }
    let Some(encoded) = offer.message.as_ref().filter(|m| m.format_major == 1) else {
        return Some(rejected(
            id,
            reason(
                verify::INVALID_MESSAGE,
                "the offer needs a format 1 message",
            ),
        ));
    };
    let bytes = &encoded.message_bytes;
    let m: mp::Message = match verify::decode(bytes, wire::MAX_MESSAGE) {
        Ok(m) => m,
        Err(r) => return Some(rejected(id, r)),
    };
    if let Err(r) = verify::check_message(&m) {
        return Some(rejected(id, r));
    }
    let origin = m.id.clone().unwrap_or_default();
    let ours = &shared.s.namespace;
    let sender_ns = m
        .sender
        .as_ref()
        .and_then(|s| s.entity.as_ref())
        .map(|e| e.namespace.as_str());
    if origin.namespace != *ours || sender_ns != Some(ours) {
        return Some(rejected(
            id,
            reason(
                verify::NOT_AUTHORIZED,
                format!("this installation asserts only senders in {ours}"),
            ),
        ));
    }
    let to = m
        .recipient
        .as_ref()
        .and_then(|r| r.entity.as_ref())
        .map(|e| e.namespace.clone())
        .unwrap_or_default();
    if to == *ours {
        return Some(rejected(
            id,
            reason(
                verify::INVALID_MESSAGE,
                format!("the recipient is in this installation's namespace {ours}"),
            ),
        ));
    }
    let expires = m.expires_at_unix_micros.unwrap_or_default();
    let now = now_micros();
    if expires <= now {
        return Some(rejected(
            id,
            reason(verify::EXPIRED, "the delivery deadline has passed"),
        ));
    }
    match shared
        .store()
        .intake_outbound(id, &new_id(), &origin, &to, bytes, expires, now)
    {
        Ok(Intake::New) => {
            tracing::info!(handoff = id, to, "took a remote send");
            None
        }
        Ok(Intake::Duplicate) => Some(pb::HandoffResult {
            handoff_id: id.clone(),
            disposition: pb::HandoffDisposition::AlreadyKnown as i32,
            ..Default::default()
        }),
        Ok(Intake::Conflict) => Some(rejected(
            id,
            reason(
                verify::MESSAGE_CONFLICT,
                format!(
                    "{}:{} or handoff {id} is already known with different content",
                    origin.namespace, origin.message_id
                ),
            ),
        )),
        Err(err) => {
            tracing::error!("storing a remote send: {err:#}");
            Some(rejected(
                id,
                reason(
                    verify::STORAGE_UNAVAILABLE,
                    "the adapter could not store it",
                ),
            ))
        }
    }
}

/// BBS's result for an import: record it and tell the relay.
fn on_incoming_result(shared: &Shared, r: &pb::HandoffResult) {
    let (state, why) = match pb::HandoffDisposition::try_from(r.disposition) {
        Ok(pb::HandoffDisposition::Received) => ("received", String::new()),
        Ok(pb::HandoffDisposition::AlreadyKnown) => ("received", "already_known".to_string()),
        Ok(pb::HandoffDisposition::Rejected) => ("rejected", code(&r.reason)),
        _ => {
            tracing::warn!(
                handoff = r.handoff_id,
                "BBS sent a result with no disposition"
            );
            return;
        }
    };
    let detail = if state == "rejected" {
        bounded(&r.detail, 1024)
    } else {
        String::new()
    };
    match shared
        .store()
        .inbound_result(&r.handoff_id, state, &why, &detail, now_micros())
    {
        Ok(Some(outcome)) => {
            tracing::info!(
                handoff = r.handoff_id,
                state,
                reason = why,
                "BBS answered an import"
            );
            shared.report_inbound(&outcome);
        }
        Ok(None) => tracing::debug!(
            handoff = r.handoff_id,
            "a result for no import awaiting one"
        ),
        Err(err) => tracing::error!("recording an import result: {err:#}"),
    }
}

/// Offer pending imports to BBS one at a time: wait for room, claim the
/// oldest (committed as attempted), then queue it. It becomes sent when the
/// transport takes it.
async fn dispatch(shared: &Arc<Shared>, tx: &link::Sender<pb::AdapterFrame>) -> Result<()> {
    loop {
        let Some(permit) = tx.reserve().await else {
            return Ok(());
        };
        let claimed = shared.store().claim_inbound(now_micros())?;
        let Some(c) = claimed else {
            drop(permit);
            tokio::select! {
                _ = shared.bbs_kick.notified() => {}
                _ = tokio::time::sleep(shared.opts.poll) => {}
            }
            continue;
        };
        let item = Item {
            frame: frame(Out::Incoming(pb::MessageOffer {
                handoff_id: c.handoff_id.clone(),
                message: Some(mp::EncodedMessage {
                    format_major: 1,
                    message_bytes: c.bytes,
                }),
            })),
            claim: Some((Direction::Inbound, c.id)),
        };
        if permit.send(item).is_err() {
            shared
                .store()
                .requeue(Direction::Inbound, c.id, now_micros())?;
            return Ok(());
        }
        tracing::info!(handoff = c.handoff_id, "offered an import to BBS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Adapter, Options, Settings};
    use ed25519_dalek::SigningKey;
    use prost::Message as _;

    fn adapter(dir: &tempfile::TempDir) -> Adapter {
        Adapter::open(
            Settings {
                namespace: "bbs".into(),
                installation_id: "bbs-1".into(),
                integration_id: "hub".into(),
                bbs_endpoint: "http://127.0.0.1:1".into(),
                relay_endpoint: "http://127.0.0.1:1".into(),
                token: "t".into(),
                key: SigningKey::from_bytes(&[4; 32]),
                credential: mp::SignedRecord::default(),
            },
            &dir.path().join("adapter.db"),
            Options::default(),
        )
        .unwrap()
    }

    fn address(namespace: &str, entity: &str) -> Option<mp::AddressRef> {
        Some(mp::AddressRef {
            entity: Some(mp::EntityRef {
                namespace: namespace.into(),
                entity_id: entity.into(),
            }),
            address_id: "native".into(),
            run_id: None,
        })
    }

    fn message(id: &str) -> mp::Message {
        mp::Message {
            id: Some(mp::MessageRef {
                namespace: "bbs".into(),
                message_id: id.into(),
            }),
            sender: address("bbs", "will/scout"),
            recipient: address("hub", "will"),
            authored_at_unix_micros: now_micros(),
            expires_at_unix_micros: Some(now_micros() + 60_000_000),
            content_type: "text/plain; charset=utf-8".into(),
            body: b"hi".to_vec(),
            kind: mp::MessageKind::Application as i32,
            ..Default::default()
        }
    }

    fn offer(handoff: &str, bytes: Vec<u8>) -> pb::MessageOffer {
        pb::MessageOffer {
            handoff_id: handoff.into(),
            message: Some(mp::EncodedMessage {
                format_major: 1,
                message_bytes: bytes,
            }),
        }
    }

    fn reason_of(r: Option<pb::HandoffResult>) -> String {
        let r = r.expect("a result now");
        assert_eq!(
            r.disposition,
            pb::HandoffDisposition::Rejected as i32,
            "{r:?}"
        );
        r.reason
    }

    #[test]
    fn outbound_offers_are_checked_before_they_are_stored() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(&dir);
        let s = &a.shared;
        let ok = message("m1").encode_to_vec();
        assert_eq!(
            reason_of(on_outgoing(s, &offer("bad id!", ok.clone()))),
            "invalid_message"
        );
        let mut v2 = offer("h0", ok.clone());
        v2.message.as_mut().unwrap().format_major = 2;
        assert_eq!(reason_of(on_outgoing(s, &v2)), "invalid_message");
        // Strict decoding: a duplicated singular field.
        let mut dup = ok.clone();
        crate::wire::put_bytes(&mut dup, 6, b"text/html");
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h1", dup))),
            "invalid_message"
        );
        let mut big = message("m2");
        big.body = vec![b'x'; 65537];
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h2", big.encode_to_vec()))),
            "frame_too_large"
        );
        let mut kind = message("m3");
        kind.kind = mp::MessageKind::DeliveryError as i32;
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h3", kind.encode_to_vec()))),
            "unsupported_content"
        );
        let mut other = message("m4");
        other.sender = address("work", "ann");
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h4", other.encode_to_vec()))),
            "route_not_authorized"
        );
        let mut local = message("m5");
        local.recipient = address("bbs", "will");
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h5", local.encode_to_vec()))),
            "invalid_message"
        );
        let mut late = message("m6");
        late.expires_at_unix_micros = Some(now_micros() - 1);
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h6", late.encode_to_vec()))),
            "expired"
        );

        // Taken: no result until the relay's.
        assert!(on_outgoing(s, &offer("h7", ok.clone())).is_none());
        let again = on_outgoing(s, &offer("h8", ok)).unwrap();
        assert_eq!(
            again.disposition,
            pb::HandoffDisposition::AlreadyKnown as i32
        );
        let mut changed = message("m1");
        changed.body = b"changed".to_vec();
        assert_eq!(
            reason_of(on_outgoing(s, &offer("h9", changed.encode_to_vec()))),
            "message_conflict"
        );
        assert_eq!(
            s.store().counts(Direction::Outbound).unwrap(),
            vec![("pending".to_string(), 1)]
        );
    }

    #[test]
    fn results_keep_within_bbs_bounds() {
        use crate::store::Outcome;
        let o = Outcome {
            id: 1,
            handoff_id: "h".into(),
            connection: String::new(),
            state: "rejected".into(),
            reason: "x".repeat(100),
            detail: "é".repeat(1000),
        };
        let r = crate::adapter::handoff_result(&o);
        assert_eq!(r.reason, "rejected");
        assert!(r.detail.len() <= 1024 && r.detail.chars().all(|c| c == 'é'));
        let known = Outcome {
            state: "received".into(),
            reason: "already_known".into(),
            ..o.clone()
        };
        assert_eq!(
            crate::adapter::transfer_result(&known).disposition,
            mp::TransferDisposition::AlreadyKnown as i32
        );
        let received = Outcome {
            state: "received".into(),
            reason: String::new(),
            ..o
        };
        let r = crate::adapter::handoff_result(&received);
        assert_eq!(r.disposition, pb::HandoffDisposition::Received as i32);
        assert!(r.reason.is_empty() && r.detail.is_empty());
    }
}
