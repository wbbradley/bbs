//! The integration stream to BBS (`bbs.integration.v1`, D085): the adapter
//! connects with its integration credential, exchanges hellos, and then
//!
//! - keeps BBS's publication catalog (its revision only grows on a stream;
//!   each stream's first catalog is taken as is, so a reset BBS is followed);
//! - takes BBS's outbound handoffs, checks them, and stores them for the
//!   relay; the relay's result goes back to BBS as the handoff's result.
//!   Every result owed to BBS is stored until the transport takes it, and
//!   sent again on the next stream if it did not; BBS keeps the first
//!   result for a handoff and ignores any other;
//! - offers verified messages from Hub to BBS as imports, each claimed
//!   before it is queued (up to [`QUEUE`] at once); BBS's result goes back
//!   to the relay on the stream the message came on. Imports sent to an
//!   earlier stream, whose results were lost with it, are offered again;
//! - asks BBS to answer Hub's directory queries for this namespace, and
//!   asks the relay BBS's queries about other namespaces.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bbs_core::hub_proto::integration as pb;
use bbs_core::hub_proto::integration::integration_client::IntegrationClient;
use bbs_core::hub_proto::msgproto_v1 as mp;
use pb::adapter_frame::Payload as Out;
use pb::bbs_frame::Payload as In;
use tokio::task::JoinSet;
use tonic::transport::Endpoint;
use tonic::{Code, Request, Status};

use crate::adapter::{Shared, bounded, code, handoff_result};
use crate::link::{self, Claim, Item};
use crate::store::{Direction, Intake, Outcome};
use crate::verify::{self, Reason, reason};
use crate::{new_id, now_micros, wire};

/// The integration protocol major this adapter speaks.
pub const PROTOCOL_MAJOR: u32 = 1;
/// The gRPC metadata carrying the integration credential.
pub const TOKEN_METADATA: &str = "bbs-integration-token";
/// Imports queued for BBS before dispatch waits.
pub const QUEUE: usize = 16;

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
///
/// When BBS refuses the credential, its file is read again (an operator may
/// have rotated it). When BBS ends the stream as superseded, another adapter
/// has connected with the same integration: this one waits the longest
/// backoff before trying again, so the two do not take turns knocking each
/// other off.
pub(crate) async fn run(shared: Arc<Shared>) -> Result<()> {
    let mut backoff = shared.opts.backoff_min;
    let mut token = shared.s.token.clone();
    loop {
        let start = Instant::now();
        let ended = session(&shared, &token).await;
        if start.elapsed() > Duration::from_secs(60) {
            backoff = shared.opts.backoff_min;
        }
        match ended {
            Ok(()) => tracing::info!("BBS stream ended"),
            Err(err) if err.downcast_ref::<Incompatible>().is_some() => return Err(err),
            Err(err) => match err.downcast_ref::<Status>().map(Status::code) {
                Some(Code::Unauthenticated) => {
                    tracing::error!(
                        bbs = shared.s.bbs_endpoint,
                        "BBS refused the integration credential: {err:#}"
                    );
                    if let Some(path) = &shared.s.token_file {
                        match crate::keys::read_token(path) {
                            Ok(t) if t != token => {
                                tracing::info!("read a new integration credential");
                                token = t;
                            }
                            Ok(_) => {}
                            Err(e) => tracing::error!("reading the integration credential: {e:#}"),
                        }
                    }
                }
                Some(Code::Aborted) => {
                    tracing::error!(
                        integration = shared.s.integration_id,
                        "BBS ended this stream for a newer one with the same integration: \
                         is another adapter running for it? Waiting {:?} before trying again",
                        shared.opts.backoff_max
                    );
                    backoff = shared.opts.backoff_max;
                }
                _ => tracing::warn!(bbs = shared.s.bbs_endpoint, "BBS stream: {err:#}"),
            },
        }
        let half = backoff / 2;
        let jitter = Duration::from_micros(rand::random_range(0..=half.as_micros() as u64));
        tokio::time::sleep(half + jitter).await;
        backoff = (backoff * 2).min(shared.opts.backoff_max);
    }
}

async fn session(shared: &Arc<Shared>, token: &str) -> Result<()> {
    let channel = Endpoint::from_shared(shared.s.bbs_endpoint.clone())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .connect()
        .await
        .context("connecting")?;
    let (tx, frames) = link::channel::<pb::AdapterFrame>(QUEUE, shared.settle());
    let _ = tx.try_send_control(frame(Out::Hello(pb::AdapterHello {
        integration_id: shared.s.integration_id.clone(),
        protocol_major: PROTOCOL_MAJOR,
        build: crate::build(),
    })));
    let mut client = IntegrationClient::new(channel)
        .max_decoding_message_size(wire::MAX_FRAME)
        .max_encoding_message_size(wire::MAX_FRAME);
    let mut req = Request::new(frames);
    let mut value: tonic::metadata::MetadataValue<_> = token
        .parse()
        .map_err(|_| anyhow::anyhow!("the integration credential is not valid metadata"))?;
    value.set_sensitive(true);
    req.metadata_mut().insert(TOKEN_METADATA, value);
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
        reoffer(shared);
        shared.bbs_kick.notify_one();
        tokio::select! {
            r = read(shared, &tx, inbound) => r,
            r = dispatch(shared, &tx) => r,
            r = report(shared, &tx) => r,
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

/// Offer again the imports whose results were lost with an earlier stream.
/// What that stream's transport took is recorded first: an import taken
/// just before it dropped is sent, so it is offered again too.
fn reoffer(shared: &Shared) {
    shared.apply_boundary();
    let reoffered = shared.store().reoffer_inbound(now_micros());
    match reoffered {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            imports = n,
            "offering again imports whose results were lost with an earlier stream"
        ),
        Err(err) => tracing::error!("re-offering imports: {err:#}"),
    }
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
    let mut snapshot = true;
    // BBS's remote directory queries, asked beside the reader; dropped,
    // and so aborted, with it when the stream ends.
    let mut queries = JoinSet::new();
    while let Some(f) = inbound.message().await? {
        while queries.try_join_next().is_some() {}
        let reply = match f.payload {
            Some(In::Hello(_)) => bail!("BBS repeated its hello"),
            Some(In::Published(catalog)) => {
                on_catalog(shared, &catalog, std::mem::take(&mut snapshot));
                None
            }
            Some(In::Outgoing(offer)) => {
                let result = on_outgoing(shared, &offer);
                shared.relay_kick.notify_one();
                result.and_then(|r| owe(shared, r)).map(Out::OutgoingResult)
            }
            Some(In::IncomingResult(result)) => {
                on_incoming_result(shared, &result);
                None
            }
            Some(In::LocalDirectoryResult(result)) => {
                shared.directory_answered(result);
                None
            }
            // Asked of the relay beside the reader, which never waits on it.
            // The result goes back on this stream's control queue; the task
            // ends with the stream, as BBS's waiting query does.
            Some(In::RemoteDirectoryQuery(q)) => {
                let (shared, tx) = (shared.clone(), tx.clone());
                queries.spawn(async move {
                    let query = q.query_id.clone();
                    let result = crate::relay::remote_directory(&shared, q).await;
                    tracing::info!(
                        query,
                        error = result.error_code,
                        entities = result.entities.len(),
                        "answered BBS's remote directory query"
                    );
                    let item = Item {
                        frame: frame(Out::RemoteDirectoryResult(result)),
                        claim: None,
                    };
                    let sent = match tx.reserve_control().await {
                        Some(permit) => permit.send(item).is_ok(),
                        None => false,
                    };
                    if !sent {
                        tracing::info!(query, "BBS's stream ended before the result went");
                    }
                });
                None
            }
            Some(In::RemoteRoomRequest(q)) => {
                tracing::debug!(
                    request = q.request_id,
                    "room requests are not forwarded yet"
                );
                None
            }
            Some(In::Ping(p)) => Some(Out::Pong(pb::Pong { nonce: p.nonce })),
            Some(In::Pong(_)) => None,
            None => {
                tracing::debug!("ignoring a frame from BBS with no payload this adapter knows");
                None
            }
        };
        if let Some(reply) = reply {
            let Some(permit) = tx.reserve_control().await else {
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

/// Keep a newer publication catalog, or a stream's first (`snapshot`)
/// whatever its revision. Its entities must all be in this namespace; one
/// that is not is ignored whole.
fn on_catalog(shared: &Shared, catalog: &pb::PublishedCatalog, snapshot: bool) {
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
    let before = shared.store().catalog_revision().ok().flatten();
    let kept = shared.store().put_catalog(catalog, snapshot, now_micros());
    if let (Ok(true), Some(b)) = (&kept, before)
        && catalog.revision < b
    {
        tracing::warn!(
            was = b,
            now = catalog.revision,
            "BBS's catalog revision went back (was its database reset?); taking its snapshot"
        );
    }
    match kept {
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
    let intake = shared
        .store()
        .intake_outbound(id, &new_id(), &origin, &to, bytes, expires, now);
    match intake {
        Ok(Intake::New) => {
            tracing::info!(handoff = id, to, "took a remote send");
            None
        }
        // The same handoff offered again: its result, if it has one, is
        // reported again; otherwise the relay's follows.
        Ok(Intake::Repeated(prior)) => prior.settled().then(|| {
            handoff_result(&Outcome {
                id: prior.id,
                handoff_id: id.clone(),
                connection: String::new(),
                state: prior.state,
                reason: prior.reason,
                detail: prior.detail,
                duplicate: false,
            })
        }),
        // A handoff ID BBS used before, for another message: no result can
        // be given under that ID without settling the first one.
        Ok(Intake::HandoffReused) => {
            tracing::error!(
                handoff = id,
                "BBS reused a handoff ID for a different message; ignoring the offer"
            );
            None
        }
        // The same message under a new handoff ID: answered as the first
        // was, rejected if it was, else already known. One whose first has
        // no result yet is owed that result once it has one.
        Ok(Intake::Duplicate(prior)) if prior.state == "rejected" => Some(pb::HandoffResult {
            handoff_id: id.clone(),
            disposition: pb::HandoffDisposition::Rejected as i32,
            reason: code(&prior.reason),
            detail: bounded(&prior.detail, 1024),
        }),
        Ok(Intake::Duplicate(prior)) if prior.settled() => Some(pb::HandoffResult {
            handoff_id: id.clone(),
            disposition: pb::HandoffDisposition::AlreadyKnown as i32,
            ..Default::default()
        }),
        Ok(Intake::Duplicate(_)) => {
            tracing::info!(
                handoff = id,
                "a duplicate send waits for its first's result"
            );
            None
        }
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

/// Store a result the adapter settled itself, to report as the relay's
/// are. One it cannot store, or one for an invalid handoff ID (which BBS
/// would refuse on every stream), is returned to send now, once.
fn owe(shared: &Shared, r: pb::HandoffResult) -> Option<pb::HandoffResult> {
    if !verify::valid_id(&r.handoff_id) {
        return Some(r);
    }
    let (state, why) = if r.disposition == pb::HandoffDisposition::AlreadyKnown as i32 {
        ("received", "already_known")
    } else {
        ("rejected", r.reason.as_str())
    };
    let owed = shared
        .store()
        .owe_report(&r.handoff_id, state, why, &r.detail, None, now_micros());
    match owed {
        Ok(()) => {
            shared.report_kick.notify_one();
            None
        }
        Err(err) => {
            tracing::error!("storing a result for BBS: {err:#}");
            Some(r)
        }
    }
}

/// Report results for BBS in order, on control slots. Each stays owed until
/// the transport takes its frame; one still queued when the stream closes,
/// or taken shortly before it ended, is reported again on the next. The
/// cursor only moves forward on a stream, and report IDs are never reused.
async fn report(shared: &Arc<Shared>, tx: &link::Sender<pb::AdapterFrame>) -> Result<()> {
    let mut after = 0;
    loop {
        let owed = shared.store().unreported(after, now_micros(), 32)?;
        if owed.is_empty() {
            tokio::select! {
                _ = shared.report_kick.notified() => {}
                _ = tokio::time::sleep(shared.opts.poll) => {}
            }
            continue;
        }
        for o in owed {
            let Some(permit) = tx.reserve_control().await else {
                return Ok(());
            };
            let item = Item {
                frame: frame(Out::OutgoingResult(handoff_result(&o))),
                claim: Some(Claim::Report(o.id)),
            };
            if permit.send(item).is_err() {
                return Ok(());
            }
            after = o.id;
            tracing::info!(
                handoff = o.handoff_id,
                state = o.state,
                reason = o.reason,
                "reported a remote send's result to BBS"
            );
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
    let recorded = shared
        .store()
        .inbound_result(&r.handoff_id, state, &why, &detail, now_micros());
    match recorded {
        Ok(Some(_)) => {
            tracing::info!(
                handoff = r.handoff_id,
                state,
                reason = why,
                "BBS answered an import"
            );
            shared.relay_report_kick.notify_one();
        }
        Ok(None) => tracing::debug!(
            handoff = r.handoff_id,
            "a result for no import awaiting one"
        ),
        Err(err) => tracing::error!("recording an import result: {err:#}"),
    }
}

/// Offer pending imports to BBS: wait for room (up to [`QUEUE`] queued),
/// claim the oldest (committed as attempted), then queue it. It becomes
/// sent when the transport takes it.
async fn dispatch(shared: &Arc<Shared>, tx: &link::Sender<pb::AdapterFrame>) -> Result<()> {
    loop {
        let Some(permit) = tx.reserve().await else {
            return Ok(());
        };
        let claimed = shared.store().claim_inbound(now_micros());
        let claimed = claimed?;
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
            claim: Some(Claim::Handoff(Direction::Inbound, c.id)),
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
        adapter_with(dir, Options::default())
    }

    fn adapter_with(dir: &tempfile::TempDir, opts: Options) -> Adapter {
        Adapter::open(
            Settings {
                namespace: "bbs".into(),
                installation_id: "bbs-1".into(),
                integration_id: "hub".into(),
                bbs_endpoint: "http://127.0.0.1:1".into(),
                relay_endpoint: "http://127.0.0.1:1".into(),
                token: "t".into(),
                token_file: None,
                key: SigningKey::from_bytes(&[4; 32]),
                credential: mp::SignedRecord::default(),
            },
            &dir.path().join("adapter.db"),
            opts,
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
        // A duplicate of it waits for that result too.
        assert!(on_outgoing(s, &offer("h8", ok.clone())).is_none());
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

        // The relay's result for h7 is owed to h7, and to h8 as already
        // known; a later duplicate is answered at once.
        {
            let mut store = s.store();
            let c = store
                .claim_outbound("conn-1", now_micros(), |m| m.to_vec())
                .unwrap()
                .unwrap();
            assert!(
                store
                    .outbound_result(&c.handoff_id, "conn-1", "received", "", "", now_micros())
                    .unwrap()
            );
            let owed = store.unreported(0, now_micros(), 10).unwrap();
            assert_eq!(
                owed.iter()
                    .map(|o| (o.handoff_id.as_str(), o.reason.as_str()))
                    .collect::<Vec<_>>(),
                vec![("h7", ""), ("h8", "already_known")]
            );
        }
        let later = on_outgoing(s, &offer("h10", ok)).unwrap();
        assert_eq!(
            later.disposition,
            pb::HandoffDisposition::AlreadyKnown as i32
        );
    }

    /// One BBS stream as the reporter sees it: a link and its reporting
    /// loop, whose frames the test takes as a transport would.
    struct Stream {
        tx: link::Sender<pb::AdapterFrame>,
        frames: link::Frames<pb::AdapterFrame>,
        reporter: tokio::task::JoinHandle<Result<()>>,
    }

    impl Stream {
        fn open(s: &Arc<Shared>) -> Stream {
            let (tx, frames) = link::channel::<pb::AdapterFrame>(QUEUE, s.settle());
            let reporter = tokio::spawn({
                let (s, tx) = (s.clone(), tx.clone());
                async move { report(&s, &tx).await }
            });
            Stream {
                tx,
                frames,
                reporter,
            }
        }

        async fn take(&mut self, s: &Shared) -> pb::HandoffResult {
            use tokio_stream::StreamExt as _;
            let taken = tokio::time::timeout(Duration::from_secs(5), self.frames.next())
                .await
                .expect("a result in time")
                .unwrap();
            s.apply_boundary();
            match taken.payload {
                Some(Out::OutgoingResult(r)) => r,
                other => panic!("a result, not {other:?}"),
            }
        }

        /// Drop the stream as a transport would, whatever is queued.
        fn drop_stream(self, s: &Shared) {
            self.reporter.abort();
            drop(self.frames);
            s.apply_boundary();
            assert!(self.tx.is_closed());
        }
    }

    /// A result still queued when its stream drops is owed, and the next
    /// stream reports it; one the transport took is owed no longer. Then
    /// Theo's repro (C1): after the first result is taken, a second one on
    /// the same stream is sent too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_queued_when_the_stream_drops_is_reported_on_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(&dir);
        let s = &a.shared;
        let mut late = message("m1");
        late.expires_at_unix_micros = Some(now_micros() - 1);
        let refused = on_outgoing(s, &offer("h1", late.encode_to_vec())).unwrap();
        assert!(owe(s, refused).is_none(), "stored, not sent at once");
        let owed = || s.store().owed_reports().unwrap();
        assert_eq!(owed(), 1);

        // The first stream queues it, then drops before the transport takes it.
        let first = Stream::open(s);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!first.tx.is_idle(), "queued");
        first.drop_stream(s);
        assert_eq!(owed(), 1, "still owed");

        // The next stream reports it again; taken, it is owed no longer.
        let mut second = Stream::open(s);
        let r = second.take(s).await;
        assert_eq!(
            (r.handoff_id.as_str(), r.disposition, r.reason.as_str()),
            ("h1", pb::HandoffDisposition::Rejected as i32, "expired")
        );
        assert_eq!(owed(), 0);

        // The same stream: a second result, owed after the first was taken.
        let mut known = message("m2");
        known.expires_at_unix_micros = Some(now_micros() - 1);
        let refused = on_outgoing(s, &offer("h2", known.encode_to_vec())).unwrap();
        assert!(owe(s, refused).is_none());
        assert_eq!(second.take(s).await.handoff_id, "h2");
        assert_eq!(owed(), 0);
        second.drop_stream(s);

        // A new stream sends both again (taken recently: BBS may have ended
        // the old stream before acting on them; it ignores repeats).
        let mut third = Stream::open(s);
        assert_eq!(third.take(s).await.handoff_id, "h1");
        assert_eq!(third.take(s).await.handoff_id, "h2");
        third.drop_stream(s);

        // An invalid handoff ID, which BBS refuses on any stream, is not kept.
        let bad = on_outgoing(s, &offer("bad id!", vec![])).unwrap();
        assert!(owe(s, bad).is_some());
        assert_eq!(owed(), 0);
    }

    /// Theo's repro (C2): import results recorded while a transport keeps
    /// taking claims from a link must not deadlock (the store is never taken
    /// inside a queue lock, nor a queue lock inside the store's).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn results_and_a_polling_transport_never_deadlock() {
        use tokio_stream::StreamExt as _;
        const N: usize = 3000;
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(&dir);
        let s = a.shared.clone();
        {
            let mut store = s.store();
            for n in 0..N {
                store
                    .intake_inbound(
                        &format!("in-{n}"),
                        &format!("relay-{n}"),
                        "conn-1",
                        &mp::MessageRef {
                            namespace: "hub".into(),
                            message_id: format!("m{n}"),
                        },
                        b"m",
                        b"e",
                        now_micros() + 60_000_000,
                        now_micros(),
                    )
                    .unwrap();
                store.claim_inbound(now_micros()).unwrap().unwrap();
            }
        }
        let (tx, mut frames) = link::channel::<Vec<u8>>(64, s.settle());
        let run = async {
            let transport = tokio::spawn(async move {
                let mut n = 0;
                while frames.next().await.is_some() {
                    n += 1;
                }
                n
            });
            let applier = tokio::spawn({
                let s = s.clone();
                async move {
                    loop {
                        s.apply_boundary();
                        tokio::task::yield_now().await;
                    }
                }
            });
            let queuing = tokio::spawn({
                let tx = tx.clone();
                async move {
                    for id in 1..=N as i64 {
                        let permit = tx.reserve_control().await.unwrap();
                        let item = Item {
                            frame: vec![0; 8],
                            claim: Some(Claim::RelayResult(id)),
                        };
                        assert!(permit.send(item).is_ok());
                    }
                }
            });
            let answering = tokio::task::spawn_blocking({
                let s = s.clone();
                move || {
                    for n in 0..N {
                        on_incoming_result(
                            &s,
                            &pb::HandoffResult {
                                handoff_id: format!("in-{n}"),
                                disposition: pb::HandoffDisposition::Received as i32,
                                ..Default::default()
                            },
                        );
                    }
                }
            });
            queuing.await.unwrap();
            answering.await.unwrap();
            while !tx.is_idle() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tx.close();
            let taken = transport.await.unwrap();
            applier.abort();
            taken
        };
        let taken = tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("no deadlock");
        assert_eq!(taken, N);
        s.apply_boundary();
        let store = s.store();
        assert_eq!(
            store.counts(Direction::Inbound).unwrap(),
            vec![("received".to_string(), N as i64)]
        );
        assert!(
            store
                .relay_unreported("conn-1", &mut Default::default(), 10)
                .unwrap()
                .is_empty()
        );
    }

    /// An import the last stream's transport took just before it dropped
    /// is offered again on the next stream, though its take was not yet
    /// recorded when BBS reconnected.
    #[tokio::test]
    async fn an_import_taken_just_before_a_drop_is_offered_again() {
        use tokio_stream::StreamExt as _;
        let dir = tempfile::tempdir().unwrap();
        let a = adapter(&dir);
        let s = &a.shared;
        let c = {
            let mut store = s.store();
            store
                .intake_inbound(
                    "in-1",
                    "relay-1",
                    "conn-1",
                    &mp::MessageRef {
                        namespace: "hub".into(),
                        message_id: "m1".into(),
                    },
                    b"m",
                    b"e",
                    now_micros() + 60_000_000,
                    now_micros(),
                )
                .unwrap();
            store.claim_inbound(now_micros()).unwrap().unwrap()
        };
        let (tx, mut frames) = link::channel::<pb::AdapterFrame>(QUEUE, s.settle());
        let item = Item {
            frame: frame(Out::Pong(pb::Pong { nonce: 1 })),
            claim: Some(Claim::Handoff(Direction::Inbound, c.id)),
        };
        assert!(tx.reserve().await.unwrap().send(item).is_ok());
        // Taken, noted, not yet recorded; then the stream drops.
        assert!(frames.next().await.is_some());
        drop(frames);
        reoffer(s);
        assert_eq!(
            s.store()
                .state(Direction::Inbound, c.id)
                .unwrap()
                .unwrap()
                .0,
            "pending",
            "offered again"
        );
    }

    /// A retention too long to count back from now keeps settled handoffs,
    /// rather than overflow and purge them all.
    #[test]
    fn an_overlong_retention_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let a = adapter_with(
            &dir,
            Options {
                retention: Some(Duration::from_secs(u64::MAX)),
                ..Options::default()
            },
        );
        let s = &a.shared;
        let mut late = message("m1");
        late.expires_at_unix_micros = Some(now_micros() + 100_000);
        assert!(on_outgoing(s, &offer("h1", late.encode_to_vec())).is_none());
        std::thread::sleep(Duration::from_millis(150));
        s.sweep();
        s.sweep();
        assert_eq!(
            s.store().counts(Direction::Outbound).unwrap(),
            vec![("rejected".to_string(), 1)]
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
            duplicate: false,
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
