//! The federation stream to the Hub relay (`msgproto.v1.Federation/Connect`),
//! as a namespace home that reaches the relay upstream: hello, trust pages
//! from the relay (the configured trust source), a signed route claim, then
//! transfers both ways, heartbeats and directory queries for this namespace.
//!
//! Outgoing frames use the link's three queues as Hub's peer does: a
//! heartbeat ahead of everything, then control frames (results, replies,
//! trust and route requests; 256 slots), then up to [`QUEUE`] transfers. A
//! relay that stops reading (frames waiting longer than the peer timeout
//! with none taken), or that is silent that long, is cut, as Hub's `watch`
//! does.
//!
//! Frames travel as raw bytes through [`RawCodec`], so every received frame
//! is validated strictly before it is decoded, and a transfer's envelope
//! stays the exact bytes it arrived in (Hub's codec does the same). A frame
//! that is not a strict encoding ends the stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bbs_core::hub_proto::msgproto_v1 as mp;
use mp::federation_frame::Payload as F;
use prost::Message as _;
use prost::bytes::{Buf as _, BufMut as _};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::transport::Endpoint;
use tonic::{Request, Status, Streaming};

use crate::adapter::{RelayLink, Shared, bounded, code, transfer_result};
use crate::link::{self, Claim, Item};
use crate::store::{Binding, Direction, Intake};
use crate::verify::{self, reason};
use crate::{new_id, now_micros, sign, wire};

/// Transfers queued for the relay before dispatch waits (Hub's
/// `transferSlots`).
const QUEUE: usize = 64;
/// The most trust pages one refresh reads.
const MAX_TRUST_PAGES: usize = 1000;

/// Raw frames: encoded by the caller, validated and decoded by
/// [`wire::parse_frame`].
#[derive(Clone, Copy, Default)]
pub struct RawCodec;

pub struct RawEncoder;
pub struct RawDecoder;

impl Encoder for RawEncoder {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        let mut out = vec![0; src.remaining()];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;

    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }

    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

fn frame(payload: F) -> Vec<u8> {
    mp::FederationFrame {
        payload: Some(payload),
    }
    .encode_to_vec()
}

/// Keep a stream to the relay, reconnecting with jittered exponential
/// backoff (reset after a healthy minute). Returns once the adapter stops,
/// after the session has ended.
pub(crate) async fn run(shared: Arc<Shared>) {
    let mut backoff = shared.opts.backoff_min;
    let mut stopping = shared.stopping.clone();
    while !shared.is_stopping() {
        let start = Instant::now();
        match session(&shared).await {
            Ok(()) => tracing::info!("relay stream ended"),
            Err(err) => tracing::warn!(relay = shared.s.relay_endpoint, "relay stream: {err:#}"),
        }
        if start.elapsed() > Duration::from_secs(60) {
            backoff = shared.opts.backoff_min;
        }
        let half = backoff / 2;
        let jitter = Duration::from_micros(rand::random_range(0..=half.as_micros() as u64));
        tokio::select! {
            _ = tokio::time::sleep(half + jitter) => {}
            _ = stopping.wait_for(|s| *s) => {}
        }
        backoff = (backoff * 2).min(shared.opts.backoff_max);
    }
}

/// One relay stream.
struct Session {
    shared: Arc<Shared>,
    tx: link::Sender<Vec<u8>>,
    /// Our hello's connection ID.
    connection: String,
    /// Replies awaited by ID: trust pages by query, route results by claim.
    waiters: Mutex<HashMap<String, oneshot::Sender<mp::FederationFrame>>>,
    last_recv: Mutex<Instant>,
    /// Per-frame work (directory answers, trust refreshes), which runs
    /// beside the reader and stops with the stream.
    tasks: Mutex<JoinSet<()>>,
}

async fn session(shared: &Arc<Shared>) -> Result<()> {
    let mut stopping = shared.stopping.clone();
    let channel = Endpoint::from_shared(shared.s.relay_endpoint.clone())?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true);
    let channel = tokio::select! {
        c = channel.connect() => c.context("connecting")?,
        _ = stopping.wait_for(|s| *s) => return Ok(()),
    };
    let connection = new_id();
    let (tx, frames) = link::channel::<Vec<u8>>(QUEUE, shared.settle());
    let _ = tx.try_send_control(frame(F::Hello(mp::PeerHello {
        protocol_major: 1,
        installation_id: shared.s.installation_id.clone(),
        connection_id: connection.clone(),
        build: crate::build(),
        serves_relay: false,
    })));
    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(wire::MAX_FRAME)
        .max_encoding_message_size(wire::MAX_FRAME);
    grpc.ready().await.context("connecting")?;
    let opened = grpc.streaming(
        Request::new(frames),
        tonic::codegen::http::uri::PathAndQuery::from_static("/msgproto.v1.Federation/Connect"),
        RawCodec,
    );
    let mut inbound = tokio::select! {
        r = opened => r.context("opening the federation stream")?.into_inner(),
        _ = stopping.wait_for(|s| *s) => return Ok(()),
    };
    let s = Arc::new(Session {
        shared: shared.clone(),
        tx: tx.clone(),
        connection: connection.clone(),
        waiters: Mutex::new(HashMap::new()),
        last_recv: Mutex::new(Instant::now()),
        tasks: Mutex::new(JoinSet::new()),
    });
    let result = tokio::select! {
        r = s.clone().serve(&mut inbound) => r,
        _ = stopping.wait_for(|s| *s) => Ok(()),
    };
    {
        let mut relay = shared.relay.lock().unwrap_or_else(PoisonError::into_inner);
        if relay.as_ref().is_some_and(|r| r.connection == connection) {
            *relay = None;
        }
    }
    // Stopping: no new work, but what is queued gets a bounded while to go
    // (CONTRACT.md: shutdown drains in-flight work up to 10 seconds).
    if shared.is_stopping() {
        let start = Instant::now();
        while !tx.is_idle() && !tx.is_closed() && start.elapsed() < shared.opts.drain {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // Nothing more goes out on this stream: queued claims return to pending.
    tx.close();
    drop(inbound);
    let mut tasks = std::mem::take(&mut *s.tasks.lock().unwrap_or_else(PoisonError::into_inner));
    tasks.shutdown().await;
    while let Some(r) = tasks.try_join_next() {
        if let Err(e) = r
            && e.is_panic()
        {
            tracing::error!("a relay stream task panicked: {e}");
        }
    }
    if let Err(err) = shared.store().session_ended(&connection, now_micros()) {
        tracing::error!("recording the relay session's end: {err:#}");
    }
    result
}

impl Session {
    async fn serve(self: Arc<Self>, inbound: &mut Streaming<Vec<u8>>) -> Result<()> {
        let timeout = self.shared.opts.reply_timeout;
        let first = tokio::time::timeout(timeout, inbound.message())
            .await
            .context("no hello from the relay")??
            .context("the relay ended the stream before its hello")?;
        let hello = match wire::parse_frame(&first)?.frame.payload {
            Some(F::Hello(h)) => h,
            _ => bail!("the relay's first frame was not a hello"),
        };
        if hello.protocol_major != 1
            || !verify::valid_id(&hello.installation_id)
            || !verify::valid_id(&hello.connection_id)
        {
            bail!(
                "the relay's hello is invalid or for protocol {}",
                hello.protocol_major
            );
        }
        self.shared.store().session_started(
            &self.connection,
            &hello.installation_id,
            now_micros(),
        )?;
        tracing::info!(
            relay = hello.installation_id,
            build = hello.build,
            "relay connected"
        );
        tokio::select! {
            r = self.clone().read(inbound) => r,
            r = self.clone().setup_and_dispatch() => r,
            r = self.clone().heartbeat() => r,
            r = self.clone().report_results() => r,
        }
    }

    /// Run per-frame work beside the reader, reaping what has finished.
    fn spawn(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(r) = tasks.try_join_next() {
            if let Err(e) = r
                && e.is_panic()
            {
                tracing::error!("a relay stream task panicked: {e}");
            }
        }
        tasks.spawn(work);
    }

    /// Queue a control frame that nothing waits on (an immediate transfer
    /// result, a directory reply). With the control queue full it is
    /// dropped, logged and counted, as Hub's peer drops it.
    fn send(&self, payload: F) {
        let what = match &payload {
            F::TransferResult(_) => "a transfer result",
            F::DirectoryAnswer(_) => "a directory answer",
            F::DirectoryUnavailable(_) => "a directory refusal",
            _ => "a frame",
        };
        if let Err(link::Refused::Full) = self.tx.try_send_control(frame(payload)) {
            let n = self
                .shared
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            tracing::warn!(
                dropped = n,
                "the relay stream's control queue is full; {what} was not sent"
            );
        }
    }

    /// Send a request on a control slot, waiting for room, and wait for its
    /// reply.
    async fn request(&self, id: &str, payload: F) -> Result<mp::FederationFrame> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_string(), tx);
        let sent = match self.tx.reserve_control().await {
            Some(permit) => permit
                .send(Item {
                    frame: frame(payload),
                    claim: None,
                })
                .is_ok(),
            None => false,
        };
        let reply = if sent {
            Some(tokio::time::timeout(self.shared.opts.reply_timeout, rx).await)
        } else {
            None
        };
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        Ok(reply
            .context("the relay stream is closed")?
            .context("no reply from the relay in time")??)
    }

    /// Send BBS's import results to the relay, on this stream only (Hub
    /// accepts a result from no other), on control slots. Each is reported
    /// once its frame is taken; one that cannot go before the stream ends is
    /// dropped with it, as informative results are.
    async fn report_results(self: Arc<Self>) -> Result<()> {
        let mut after = 0;
        loop {
            let owed = self
                .shared
                .store()
                .relay_unreported(&self.connection, after, 32);
            let owed = owed?;
            if owed.is_empty() {
                tokio::select! {
                    _ = self.shared.relay_report_kick.notified() => {}
                    _ = tokio::time::sleep(self.shared.opts.poll) => {}
                }
                continue;
            }
            for o in owed {
                let Some(permit) = self.tx.reserve_control().await else {
                    return Ok(());
                };
                let item = Item {
                    frame: frame(F::TransferResult(transfer_result(&o))),
                    claim: Some(Claim::RelayResult(o.id)),
                };
                if permit.send(item).is_err() {
                    return Ok(());
                }
                after = o.id;
                tracing::info!(
                    handoff = o.handoff_id,
                    state = o.state,
                    reason = o.reason,
                    "reported an import's result to the relay"
                );
            }
        }
    }

    fn deliver(&self, id: &str, f: mp::FederationFrame) {
        if let Some(w) = self
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
        {
            let _ = w.send(f);
        }
    }

    /// Refresh trust, claim the route, then hand off pending work.
    async fn setup_and_dispatch(self: Arc<Self>) -> Result<()> {
        self.refresh_trust("").await.context("trust refresh")?;
        self.claim_route().await?;
        *self
            .shared
            .relay
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(RelayLink {
            tx: self.tx.clone(),
            connection: self.connection.clone(),
        });
        self.shared
            .store()
            .session_ready(&self.connection, now_micros())?;
        tracing::info!(
            connection = self.connection,
            "route registered; federation is ready"
        );
        self.dispatch().await
    }

    /// Page trust from the relay and apply it. Cached trust stays usable if
    /// this fails.
    async fn refresh_trust(&self, namespace: &str) -> Result<()> {
        let mut token = Vec::new();
        for _ in 0..MAX_TRUST_PAGES {
            let id = new_id();
            let reply = self
                .request(
                    &id,
                    F::TrustQuery(mp::TrustQuery {
                        query_id: id.clone(),
                        namespace: namespace.to_string(),
                        page_token: token,
                    }),
                )
                .await?;
            let Some(F::TrustPage(page)) = reply.payload else {
                bail!("the relay answered a trust query with something else");
            };
            if !page.error_code.is_empty() {
                bail!("trust page: {}", page.error_code);
            }
            self.apply_trust(&page)?;
            if page.next_page_token.is_empty() || !namespace.is_empty() {
                return Ok(());
            }
            token = page.next_page_token;
        }
        bail!("the relay's trust pages did not end")
    }

    fn apply_trust(&self, page: &mp::TrustPage) -> Result<()> {
        let now = now_micros();
        let store = self.shared.store();
        for b in &page.bindings {
            // A bad binding is skipped, not the page: its revocations still
            // apply.
            if !verify::valid_namespace(&b.namespace)
                || b.authority_public_key.len() != 32
                || b.revision == 0
            {
                tracing::warn!(namespace = b.namespace, "skipping a malformed binding");
                continue;
            }
            if !sign::usable_key(&b.authority_public_key) {
                tracing::warn!(
                    namespace = b.namespace,
                    "skipping a binding whose authority key is not a usable Ed25519 key \
                     (not a point, or of small order)"
                );
                continue;
            }
            let binding = Binding {
                namespace: b.namespace.clone(),
                authority_key: b.authority_public_key.clone(),
                revision: b.revision,
                disabled: b.disabled,
            };
            match store.put_binding(&binding, now) {
                Ok(true) => tracing::info!(
                    namespace = b.namespace,
                    revision = b.revision,
                    disabled = b.disabled,
                    "trusted namespace binding"
                ),
                Ok(false) => {}
                // A conflict keeps the stored binding.
                Err(err) => tracing::warn!("binding {}: {err:#}", b.namespace),
            }
        }
        for rec in &page.revocations {
            match verify::revocation(&store, rec) {
                Ok(r) => {
                    if store.put_revocation(&r.namespace, &r.public_key, rec, now)? {
                        tracing::warn!(namespace = r.namespace, "a key was revoked");
                    }
                }
                Err(r) => tracing::warn!("revocation: {r}"),
            }
        }
        for rec in &page.credentials {
            if let Err(r) = verify::credential(&store, rec) {
                tracing::warn!("credential: {r}");
            }
        }
        Ok(())
    }

    /// Claim this namespace's route on the relay (HOST_NAMESPACE).
    async fn claim_route(&self) -> Result<()> {
        let claim_id = new_id();
        let claim = mp::RouteClaim {
            namespace: self.shared.s.namespace.clone(),
            installation_id: self.shared.s.installation_id.clone(),
            connection_id: self.connection.clone(),
            claim_id: claim_id.clone(),
        };
        let record = sign::sign(
            &self.shared.s.key,
            mp::RecordKind::RouteClaim,
            claim.encode_to_vec(),
        );
        let reply = self
            .request(
                &claim_id,
                F::RegisterRoute(mp::RegisterRoute {
                    claim: Some(record),
                    credential: Some(self.shared.s.credential.clone()),
                }),
            )
            .await
            .context("route claim")?;
        match reply.payload {
            Some(F::RouteResult(r)) if r.accepted => Ok(()),
            Some(F::RouteResult(r)) => bail!(
                "the relay refused the route for {}: {} {}",
                self.shared.s.namespace,
                r.error_code,
                r.detail
            ),
            _ => bail!("the relay answered a route claim with something else"),
        }
    }

    /// Hand off pending outbound work: wait for room (up to [`QUEUE`]
    /// transfers queued), claim the oldest (committed as attempted, its
    /// envelope signed at the first claim), then queue it. It becomes sent
    /// when the transport takes it.
    async fn dispatch(&self) -> Result<()> {
        let shared = &self.shared;
        loop {
            let Some(permit) = self.tx.reserve().await else {
                return Ok(());
            };
            let claimed =
                shared
                    .store()
                    .claim_outbound(&self.connection, now_micros(), |message| {
                        seal(shared, message)
                    });
            let claimed = claimed?;
            let Some(c) = claimed else {
                drop(permit);
                tokio::select! {
                    _ = shared.relay_kick.notified() => {}
                    _ = tokio::time::sleep(shared.opts.poll) => {}
                }
                continue;
            };
            let item = Item {
                frame: wire::transfer_frame(&c.handoff_id, &c.bytes),
                claim: Some(Claim::Handoff(Direction::Outbound, c.id)),
            };
            if permit.send(item).is_err() {
                shared
                    .store()
                    .requeue(Direction::Outbound, c.id, now_micros())?;
                return Ok(());
            }
            tracing::info!(handoff = c.handoff_id, "queued a transfer for the relay");
        }
    }

    async fn heartbeat(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(self.shared.opts.heartbeat);
        tick.tick().await;
        let mut nonce = 0u64;
        loop {
            tick.tick().await;
            let silent = self
                .last_recv
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .elapsed();
            if silent > self.shared.opts.peer_timeout {
                bail!("the relay was silent for {silent:?}");
            }
            // Frames waiting this long with none taken: flow control is full,
            // so the relay is not reading, whatever heartbeats it sends.
            let stalled = self.tx.stalled_for();
            if stalled > self.shared.opts.peer_timeout {
                bail!("the relay is not reading: frames have waited {stalled:?} to be sent");
            }
            nonce += 1;
            // Ahead of queued transfers, so a full queue never silences us.
            self.tx
                .try_send_priority(frame(F::Heartbeat(mp::Heartbeat { nonce })));
        }
    }

    async fn read(self: Arc<Self>, inbound: &mut Streaming<Vec<u8>>) -> Result<()> {
        while let Some(raw) = inbound.message().await? {
            *self
                .last_recv
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Instant::now();
            let parsed = wire::parse_frame(&raw).context("a frame from the relay")?;
            match parsed.frame.payload {
                Some(F::Hello(_)) => bail!("the relay repeated its hello"),
                Some(F::Heartbeat(_)) => {}
                Some(F::RouteResult(r)) => {
                    let id = r.claim_id.clone();
                    self.deliver(
                        &id,
                        mp::FederationFrame {
                            payload: Some(F::RouteResult(r)),
                        },
                    );
                }
                Some(F::TrustPage(p)) => {
                    let id = p.query_id.clone();
                    self.deliver(
                        &id,
                        mp::FederationFrame {
                            payload: Some(F::TrustPage(p)),
                        },
                    );
                }
                // Refreshed until it succeeds or the stream ends: a revocation
                // must not wait for the next stream.
                Some(F::TrustChanged(c)) => {
                    let s = self.clone();
                    self.spawn(async move {
                        let mut wait = s.shared.opts.backoff_min;
                        while let Err(err) = s.refresh_trust(&c.namespace).await {
                            tracing::warn!(
                                namespace = c.namespace,
                                "trust refresh after a change: {err:#}; retrying in {wait:?}"
                            );
                            tokio::time::sleep(wait).await;
                            wait = (wait * 2).min(s.shared.opts.backoff_max);
                        }
                    });
                }
                Some(F::Transfer(t)) => {
                    self.on_transfer(t, parsed.envelope.unwrap_or_default());
                }
                Some(F::TransferResult(r)) => self.on_transfer_result(r),
                Some(F::DirectoryQuery(q)) => {
                    if let Some((rec, q)) = self.check_directory_query(q) {
                        let s = self.clone();
                        self.spawn(async move { s.answer_directory(rec, q).await });
                    }
                }
                Some(other) => {
                    tracing::debug!("ignoring an unexpected frame from the relay: {other:?}")
                }
                None => tracing::debug!("ignoring a frame with no payload this adapter knows"),
            }
        }
        Ok(())
    }

    /// Admit one transfer for BBS: verify the envelope from its exact bytes,
    /// then store it pending. Its result comes from BBS's import; a
    /// duplicate or conflict is answered here, and BBS is not offered it
    /// again.
    fn on_transfer(&self, t: mp::Transfer, raw: Vec<u8>) {
        let shared = &self.shared;
        let reply = |disposition: mp::TransferDisposition, code_: &str, detail: &str| {
            self.send(F::TransferResult(mp::TransferResult {
                handoff_id: t.handoff_id.clone(),
                disposition: disposition as i32,
                error_code: code_.to_string(),
                detail: bounded(detail, 1024),
            }));
        };
        let reject = |r: verify::Reason| {
            tracing::info!(
                handoff = t.handoff_id,
                reason = r.code,
                "refused a transfer: {}",
                r.detail
            );
            reply(mp::TransferDisposition::Rejected, r.code, &r.detail);
        };
        if !verify::valid_id(&t.handoff_id) {
            return reject(reason(verify::INVALID_MESSAGE, "invalid handoff ID"));
        }
        let verified = {
            let store = shared.store();
            verify::envelope(&store, &raw)
        };
        let v = match verified {
            Ok(v) => v,
            Err(r) => return reject(r),
        };
        let m = &v.message;
        let id = m.id.clone().unwrap_or_default();
        let ours = &shared.s.namespace;
        if id.namespace == *ours {
            return reject(reason(
                verify::NOT_AUTHORIZED,
                format!("messages from {ours} originate only at its home"),
            ));
        }
        let to = m
            .recipient
            .as_ref()
            .and_then(|r| r.entity.as_ref())
            .map(|e| e.namespace.clone())
            .unwrap_or_default();
        if to != *ours {
            return reject(reason(
                verify::NOT_AUTHORIZED,
                format!("this home does not relay to {to}"),
            ));
        }
        let intake = shared.store().intake_inbound(
            &new_id(),
            &t.handoff_id,
            &self.connection,
            &id,
            &v.bytes,
            &raw,
            m.expires_at_unix_micros.unwrap_or_default(),
            now_micros(),
        );
        match intake {
            Ok(Intake::New) => {
                tracing::info!(
                    handoff = t.handoff_id,
                    origin = format!("{}:{}", id.namespace, id.message_id),
                    "received a transfer for BBS"
                );
                shared.bbs_kick.notify_one();
            }
            // The same message again: answered as the first was, rejected
            // if it was (expired, or refused by BBS), else already known.
            Ok(Intake::Duplicate(prior)) if prior.state == "rejected" => {
                let code_ = crate::adapter::relay_code(&prior.reason);
                reply(mp::TransferDisposition::Rejected, &code_, &prior.detail)
            }
            Ok(Intake::Duplicate(_)) => reply(mp::TransferDisposition::AlreadyKnown, "", ""),
            // Outbound only.
            Ok(Intake::Repeated(_) | Intake::HandoffReused) => {}
            Ok(Intake::Conflict) => reject(reason(
                verify::MESSAGE_CONFLICT,
                format!(
                    "{}:{} is already known with different content",
                    id.namespace, id.message_id
                ),
            )),
            Err(err) => {
                tracing::error!("storing a transfer: {err:#}");
                reject(reason(
                    verify::STORAGE_UNAVAILABLE,
                    "the adapter could not store it",
                ));
            }
        }
    }

    /// The relay's informative result for one of our transfers, accepted
    /// only on the stream it was attempted on.
    fn on_transfer_result(&self, r: mp::TransferResult) {
        let (state, why) = match mp::TransferDisposition::try_from(r.disposition) {
            Ok(mp::TransferDisposition::Received) => ("received", String::new()),
            Ok(mp::TransferDisposition::AlreadyKnown) => ("received", "already_known".to_string()),
            // Rejected, and any disposition this adapter does not know (D108).
            _ => ("rejected", code(&r.error_code)),
        };
        let detail = if state == "rejected" {
            bounded(&r.detail, 1024)
        } else {
            String::new()
        };
        let recorded = self.shared.store().outbound_result(
            &r.handoff_id,
            &self.connection,
            state,
            &why,
            &detail,
            now_micros(),
        );
        match recorded {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    handoff = r.handoff_id,
                    "a result for no transfer attempted on this stream"
                );
                return;
            }
            Err(err) => {
                tracing::error!("recording a transfer result: {err:#}");
                return;
            }
        }
        tracing::info!(
            handoff = r.handoff_id,
            state,
            reason = why,
            "the relay answered a transfer"
        );
        self.shared.report_kick.notify_one();
    }

    fn unavailable(&self, query_id: &str, code_: &str, detail: &str) {
        self.send(F::DirectoryUnavailable(mp::DirectoryUnavailable {
            query_id: query_id.to_string(),
            error_code: code_.to_string(),
            detail: bounded(detail, 1024),
        }));
    }

    /// Check a directory query in Hub's order, refusing what fails. Returns
    /// the query to answer: one for this namespace, before its deadline.
    fn check_directory_query(
        &self,
        fq: mp::FederatedDirectoryQuery,
    ) -> Option<(mp::SignedRecord, mp::DirectoryQuery)> {
        let shared = &self.shared;
        let (Some(rec), Some(cred)) = (fq.query, &fq.credential) else {
            return None;
        };
        let Ok(q) = verify::decode::<mp::DirectoryQuery>(&rec.payload, wire::MAX_DIRECTORY) else {
            return None;
        };
        if !verify::valid_id(&q.query_id) {
            return None;
        }
        let checked = {
            let store = shared.store();
            verify::credential(&store, cred).and_then(|c| {
                verify::subject(&rec, mp::RecordKind::DirectoryQuery, &c)?;
                let who = q.requester.as_ref().and_then(|r| r.entity.as_ref());
                if !c.assert_senders
                    || who.is_none_or(|e| {
                        e.namespace != c.namespace || !verify::valid_entity_id(&e.entity_id)
                    })
                {
                    return Err(reason(
                        verify::NOT_AUTHORIZED,
                        "query requester is not asserted by its credential",
                    ));
                }
                if !q.alias.is_empty() && !q.entity_id.is_empty() {
                    return Err(reason(
                        verify::INVALID_MESSAGE,
                        "alias and entity_id are mutually exclusive",
                    ));
                }
                Ok(())
            })
        };
        if let Err(r) = checked {
            // A store failure takes Hub's fallback for a query that fails
            // verification (asReason(err, RInvalidSignature)).
            let code_ = if r.code == verify::STORAGE_UNAVAILABLE {
                verify::INVALID_SIGNATURE
            } else {
                r.code
            };
            self.unavailable(&q.query_id, code_, &r.detail);
            return None;
        }
        if now_micros() > q.deadline_unix_micros {
            return None; // late: the asker has given up
        }
        if q.namespace != shared.s.namespace {
            self.unavailable(
                &q.query_id,
                verify::NAMESPACE_UNKNOWN,
                &format!("this home does not serve {}", q.namespace),
            );
            return None;
        }
        Some((rec, q))
    }

    /// Answer a checked directory query for this namespace with BBS's
    /// answer, signed and bound to the query (HOST_NAMESPACE).
    async fn answer_directory(self: Arc<Self>, rec: mp::SignedRecord, q: mp::DirectoryQuery) {
        let shared = &self.shared;
        let now = now_micros();
        let deadline = q.deadline_unix_micros.min(now + 30_000_000);
        let query_id = q.query_id.clone();
        let Some(mut result) = shared.local_directory(q, deadline).await else {
            tracing::info!(
                query = query_id,
                "BBS did not answer a directory query in time"
            );
            return;
        };
        result.query_id = query_id.clone();
        let Some(hash) = sign::hash(&rec) else {
            return;
        };
        let mut answer = mp::DirectoryAnswer {
            query_record_hash: hash.to_vec(),
            namespace: shared.s.namespace.clone(),
            result: Some(result),
        };
        if answer.encoded_len() > wire::MAX_DIRECTORY {
            answer.result = Some(mp::DirectoryResult {
                query_id,
                error_code: verify::FRAME_TOO_LARGE.to_string(),
                detail: "the directory page is too large".to_string(),
                ..Default::default()
            });
        }
        let signed = sign::sign(
            &shared.s.key,
            mp::RecordKind::DirectoryAnswer,
            answer.encode_to_vec(),
        );
        self.send(F::DirectoryAnswer(mp::FederatedDirectoryAnswer {
            answer: Some(signed),
            credential: Some(shared.s.credential.clone()),
        }));
    }
}

/// The envelope for a message BBS froze: its exact bytes signed as a
/// MESSAGE record, with this installation's credential.
pub(crate) fn seal(shared: &Shared, message: &[u8]) -> Vec<u8> {
    mp::SignedEnvelope {
        message: Some(sign::sign(
            &shared.s.key,
            mp::RecordKind::Message,
            message.to_vec(),
        )),
        credential: Some(shared.s.credential.clone()),
    }
    .encode_to_vec()
}
