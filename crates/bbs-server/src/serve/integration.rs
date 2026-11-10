//! Hub's integration stream (`bbs.integration.v1`) on its own listener,
//! `bbs serve --integration-listen`. The adapter connects with an integration
//! credential, sends its hello, and the stream carries both directions: BBS
//! publishes its catalog and hands off outbound work; the adapter offers
//! incoming messages, reports results and asks directory queries. Rules and
//! storage are in `service::integration`; this runs the stream.
//!
//! One stream per integration: a new connection supersedes the older one,
//! which ends. Outbound work is claimed (marked attempted and committed)
//! before it is queued for the stream, and is never offered again (D072). A
//! claim becomes `sent` only when the transport takes its frame; a stream
//! that ends first leaves its queued claims `unknown`.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

use super::{BoxStream, Policy, State};
use crate::service::integration::{self, Catalog, InvalidFrame, TOKEN_METADATA, Unsupported};
use bbs_core::hub_proto::integration as pb;
use pb::adapter_frame::Payload as In;
use pb::bbs_frame::Payload as Out;
use pb::integration_server::{Integration as IntegrationRpc, IntegrationServer};

/// How long a new stream may take to send its hello.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often an open stream rechecks its credential, and the catalog, which
/// local administration can change without the server noticing.
const RECHECK: Duration = Duration::from_secs(5);
/// Frames queued for the adapter before senders wait.
const OUTBOUND_FRAMES: usize = 16;
/// Streams between connecting and a validated hello, across all peers.
const MAX_HANDSHAKES: usize = 16;
/// Concurrent HTTP/2 streams on one integration connection.
pub(super) const MAX_STREAMS_PER_CONNECTION: u32 = 4;
/// How often pending handoffs past their deadline are expired.
const SWEEP: Duration = Duration::from_secs(60);
/// Why queued claims that never reached the transport are unknown.
const NOT_HANDED_OFF: &str = "the stream ended before the frame reached the transport";

/// Open streams by integration ID; setting one's flag supersedes it.
type Sessions = Arc<Mutex<HashMap<String, Arc<watch::Sender<bool>>>>>;

/// A frame for the adapter, with the outbox row it hands off, if any.
struct Item {
    frame: pb::BbsFrame,
    claim: Option<i64>,
}

type Queue = Arc<Mutex<mpsc::Receiver<Item>>>;

#[derive(Clone)]
pub(super) struct Integration {
    state: Arc<State>,
    sessions: Sessions,
    /// A read-only connection for credential checks, so unauthenticated
    /// peers never wait on, or hold, the shared one. None for in-memory
    /// databases.
    auth: Option<Arc<Mutex<Connection>>>,
    handshakes: Arc<Semaphore>,
}

/// The integration service, bounded to Hub's 4 MiB frames each way.
/// Credentials are checked on a read-only connection to `db` when given.
pub(super) fn service(
    state: Arc<State>,
    db: Option<&Path>,
) -> anyhow::Result<IntegrationServer<Integration>> {
    let auth = match db {
        Some(path) => {
            let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening {} for credential checks", path.display()))?;
            conn.busy_timeout(Duration::from_secs(5))?;
            Some(Arc::new(Mutex::new(conn)))
        }
        None => None,
    };
    Ok(IntegrationServer::new(Integration {
        state,
        sessions: Arc::default(),
        auth,
        handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
    })
    .max_decoding_message_size(integration::MAX_FRAME_BYTES)
    .max_encoding_message_size(integration::MAX_FRAME_BYTES))
}

/// The status the adapter sees for an error. Only messages BBS wrote for the
/// adapter pass through; anything else is logged and reported as internal.
fn status(err: anyhow::Error) -> Status {
    if let Some(e) = err.downcast_ref::<crate::auth::AuthenticationError>() {
        Status::unauthenticated(e.to_string())
    } else if let Some(e) = err.downcast_ref::<InvalidFrame>() {
        Status::invalid_argument(e.to_string())
    } else if let Some(e) = err.downcast_ref::<Unsupported>() {
        Status::failed_precondition(e.to_string())
    } else {
        tracing::error!("integration stream: {err:#}");
        Status::internal("internal error")
    }
}

/// Run `f` on the shared connection off the runtime, as `State::call` does,
/// with [`status`]'s errors.
async fn call<T, F>(state: &Arc<State>, f: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
{
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if *state.shutdown.borrow() {
            return Err(Status::unavailable("server is shutting down"));
        }
        f(&mut conn).map_err(status)
    })
    .await
    .map_err(|err| {
        tracing::error!("integration call failed: {err}");
        Status::internal("internal error")
    })?
}

/// Record outbox rows in the background: the stream cannot wait for them.
fn record(state: &Arc<State>, claims: Vec<i64>, sent: bool) {
    if claims.is_empty() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // No runtime: the claims stay attempted until the next start makes
        // them unknown.
        return;
    };
    let state = state.clone();
    runtime.spawn_blocking(move || {
        let mut conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let result = if sent {
            claims
                .iter()
                .try_for_each(|&id| integration::handed_off(&conn, id))
        } else {
            integration::mark_unknown(&mut conn, &claims, NOT_HANDED_OFF)
        };
        drop(conn);
        match result {
            // An unknown handoff mails its sender a notice.
            Ok(()) if !sent => state.changed(),
            Ok(()) => {}
            Err(err) => tracing::error!("recording handoffs: {err:#}"),
        }
    });
}

/// Close the queue and drop what it holds: none of it reaches the adapter,
/// and its claims become unknown.
fn abandon(queue: &Queue, state: &Arc<State>) {
    let mut rx = queue.lock().unwrap_or_else(PoisonError::into_inner);
    rx.close();
    let mut claims = Vec::new();
    while let Ok(item) = rx.try_recv() {
        claims.extend(item.claim);
    }
    record(state, claims, false);
}

fn frame(payload: Out) -> pb::BbsFrame {
    pb::BbsFrame {
        payload: Some(payload),
    }
}

/// The response stream. A frame counts as handed off when the transport
/// takes it from here; a terminal status ends the stream at once, ahead of
/// anything still queued, which is abandoned.
struct FrameStream {
    queue: Queue,
    terminal: Option<oneshot::Receiver<Status>>,
    done: bool,
    protocol: Policy,
    state: Arc<State>,
}

impl Stream for FrameStream {
    type Item = Result<pb::BbsFrame, Status>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        use std::future::Future;
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(terminal) = &mut this.terminal {
            match Pin::new(terminal).poll(cx) {
                Poll::Ready(Ok(status)) => {
                    this.done = true;
                    abandon(&this.queue, &this.state);
                    return Poll::Ready(Some(Err(this.protocol.status(status))));
                }
                Poll::Ready(Err(_)) => this.terminal = None,
                Poll::Pending => {}
            }
        }
        let polled = this
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .poll_recv(cx);
        match polled {
            Poll::Ready(Some(item)) => {
                record(&this.state, item.claim.into_iter().collect(), true);
                Poll::Ready(Some(Ok(item.frame)))
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        abandon(&self.queue, &self.state);
    }
}

impl Integration {
    /// Register a new stream for `id`, ending any older one.
    fn supersede(&self, id: &str) -> Arc<watch::Sender<bool>> {
        let mine = Arc::new(watch::Sender::new(false));
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = sessions.insert(id.to_string(), mine.clone()) {
            old.send_replace(true);
        }
        mine
    }

    fn release(&self, id: &str, mine: &Arc<watch::Sender<bool>>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if sessions.get(id).is_some_and(|s| Arc::ptr_eq(s, mine)) {
            sessions.remove(id);
        }
    }

    /// The enabled integration `token` authenticates, checked on the
    /// read-only connection when there is one.
    async fn authenticate(&self, token: Option<String>) -> Result<String, Status> {
        let Some(auth) = self.auth.clone() else {
            return call(&self.state, move |conn| {
                integration::authenticate(conn, token.as_deref())
            })
            .await;
        };
        tokio::task::spawn_blocking(move || {
            let conn = auth.lock().unwrap_or_else(PoisonError::into_inner);
            integration::authenticate(&conn, token.as_deref()).map_err(status)
        })
        .await
        .map_err(|_| Status::internal("internal error"))?
    }

    /// Recheck an open stream's credential every [`RECHECK`]; returns the
    /// status that ends it.
    async fn recheck(&self, id: &str, token: &str) -> Status {
        loop {
            tokio::time::sleep(RECHECK).await;
            match self.authenticate(Some(token.to_string())).await {
                Ok(still) if still == id => {}
                Ok(_) => {
                    return Status::unauthenticated("invalid or revoked integration credential");
                }
                Err(status) => return status,
            }
        }
    }
}

#[tonic::async_trait]
impl IntegrationRpc for Integration {
    type ConnectStream = BoxStream<pb::BbsFrame>;

    async fn connect(
        &self,
        req: Request<Streaming<pb::AdapterFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let state = self.state.clone();
        let peer = req.remote_addr().map(|addr| addr.ip());
        let handshake = self.handshakes.clone().try_acquire_owned().map_err(|_| {
            Status::resource_exhausted("too many integration streams are connecting")
        })?;
        let token = req
            .metadata()
            .get(TOKEN_METADATA)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // A limited address still gets through with a valid credential.
        let limited = state.check_auth_rate(peer).err();
        let id = match self.authenticate(token.clone()).await {
            Ok(id) => id,
            Err(status) => {
                state.record_auth_failure(peer, &status);
                tracing::warn!(?peer, "integration stream refused: {}", status.message());
                return Err(limited.unwrap_or(status));
            }
        };
        let token = token.unwrap_or_default();
        let mut inbound = req.into_inner();
        let first = tokio::time::timeout(HELLO_WAIT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no hello within 10 seconds"))??;
        let hello = match first.and_then(|f| f.payload) {
            Some(In::Hello(hello)) => hello,
            _ => return Err(Status::invalid_argument("the first frame must be hello")),
        };
        integration::check_hello(&id, &hello).map_err(|err| match err
            .downcast_ref::<crate::auth::AuthenticationError>(
        ) {
            Some(e) => Status::permission_denied(e.to_string()),
            None => status(err),
        })?;
        let i = id.clone();
        let bbs_hello = call(&state, move |conn| integration::hello(conn, &i)).await?;
        drop(handshake);
        let superseded = self.supersede(&id);
        tracing::info!(
            integration = id,
            build = hello.build,
            ?peer,
            "integration connected"
        );

        let (tx, rx) = mpsc::channel(OUTBOUND_FRAMES);
        // Room for the hello is certain: the channel is new.
        let _ = tx.try_send(Item {
            frame: frame(Out::Hello(bbs_hello)),
            claim: None,
        });
        let queue: Queue = Arc::new(Mutex::new(rx));
        let (terminal, terminal_rx) = oneshot::channel();
        let this = self.clone();
        let session_queue = queue.clone();
        tokio::spawn(async move {
            let mut ended = superseded.subscribe();
            let session = Session {
                state: state.clone(),
                id: id.clone(),
                tx,
            };
            // Every way the stream ends is here, so each can cut short the
            // others, including a handoff waiting for room on the stream.
            let result = tokio::select! {
                r = session.read(inbound) => r,
                r = session.write() => r,
                status = this.recheck(&id, &token) => Err(status),
                _ = state.stopping() => Err(Status::unavailable("server is shutting down")),
                _ = ended.wait_for(|s| *s) => {
                    Err(Status::aborted("superseded by a newer stream for this integration"))
                }
                _ = session.tx.closed() => Ok(()),
            };
            match result {
                Ok(()) => tracing::info!(integration = id, "integration disconnected"),
                Err(status) => {
                    tracing::warn!(
                        integration = id,
                        "integration stream ended: {}",
                        status.message()
                    );
                    // Whatever is still queued never reaches the adapter.
                    abandon(&session_queue, &state);
                    let _ = terminal.send(status);
                }
            }
            this.release(&id, &superseded);
        });
        Ok(Response::new(Box::pin(FrameStream {
            queue,
            terminal: Some(terminal_rx),
            done: false,
            protocol: self.state.protocol.clone(),
            state: self.state.clone(),
        }) as BoxStream<pb::BbsFrame>))
    }
}

/// Expire pending handoffs past their deadline, at startup and then every
/// [`SWEEP`], telling their senders.
pub(super) async fn sweep(state: Arc<State>) {
    loop {
        let now = (state.clock)();
        match call(&state, move |conn| integration::expire(conn, now)).await {
            Ok(0) => {}
            Ok(expired) => {
                tracing::info!(expired, "expired pending remote sends");
                state.changed();
            }
            Err(status) if status.code() == tonic::Code::Unavailable => return,
            Err(status) => tracing::warn!("expiring remote sends: {}", status.message()),
        }
        tokio::select! {
            _ = tokio::time::sleep(SWEEP) => {}
            _ = state.stopping() => return,
        }
    }
}

/// One open stream.
struct Session {
    state: Arc<State>,
    id: String,
    tx: mpsc::Sender<Item>,
}

impl Session {
    /// Queue a frame; false when the stream has gone.
    async fn send(&self, payload: Out) -> bool {
        self.tx
            .send(Item {
                frame: frame(payload),
                claim: None,
            })
            .await
            .is_ok()
    }

    /// Handle the adapter's frames until it ends the stream.
    async fn read(&self, mut inbound: Streaming<pb::AdapterFrame>) -> Result<(), Status> {
        while let Some(f) = inbound.message().await? {
            let id = self.id.clone();
            let reply = match f.payload {
                None => {
                    return Err(Status::invalid_argument(
                        "a frame has no payload, or one this server does not know",
                    ));
                }
                Some(In::Hello(_)) => return Err(Status::invalid_argument("repeated hello")),
                Some(In::Incoming(offer)) => {
                    let result = call(&self.state, move |conn| {
                        Ok(integration::import(conn, &id, &offer))
                    })
                    .await?;
                    if result.disposition == pb::HandoffDisposition::Received as i32 {
                        self.state.changed();
                    }
                    Some(Out::IncomingResult(result))
                }
                Some(In::OutgoingResult(result)) => {
                    let changed = call(&self.state, move |conn| {
                        integration::record_result(conn, &id, &result)
                    })
                    .await?;
                    // A rejection mails its sender a notice.
                    if changed {
                        self.state.changed();
                    }
                    None
                }
                Some(In::Observed(observed)) => {
                    call(&self.state, move |conn| {
                        integration::observe(conn, &id, &observed)
                    })
                    .await?;
                    None
                }
                Some(In::LocalDirectoryQuery(query)) => Some(Out::LocalDirectoryResult(
                    call(&self.state, move |conn| {
                        integration::local_directory(conn, &id, &query)
                    })
                    .await?,
                )),
                Some(In::RemoteDirectoryResult(result)) => {
                    integration::check_remote_directory(&result).map_err(status)?;
                    None
                }
                Some(In::Ping(ping)) => Some(Out::Pong(pb::Pong { nonce: ping.nonce })),
                Some(In::Pong(_)) => None,
            };
            if let Some(reply) = reply
                && !self.send(reply).await
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Publish the catalog when it may have changed and hand off pending
    /// work, after every committed change and every [`RECHECK`]. The catalog
    /// is rebuilt only when its key (the native entities' epoch and the
    /// publication switch) moves; one too large for a frame is logged, not
    /// sent, and does not hold up the outbox.
    async fn write(&self) -> Result<(), Status> {
        let mut changes = self.state.notify.subscribe();
        let mut recheck = tokio::time::interval(RECHECK);
        recheck.tick().await;
        let mut key = None;
        let mut published = None;
        loop {
            changes.borrow_and_update();
            let id = self.id.clone();
            let now = call(&self.state, move |conn| integration::catalog_key(conn, &id)).await?;
            if key != Some(now) {
                key = Some(now);
                let id = self.id.clone();
                match call(&self.state, move |conn| integration::catalog(conn, &id)).await? {
                    Catalog::Snapshot(catalog) => {
                        if published != Some(catalog.revision) {
                            published = Some(catalog.revision);
                            if !self.send(Out::Published(catalog)).await {
                                return Ok(());
                            }
                        }
                    }
                    Catalog::TooLarge { entities, bytes } => tracing::error!(
                        integration = self.id,
                        entities,
                        bytes,
                        "the publication catalog is over one frame; not published"
                    ),
                }
            }
            if !self.hand_off().await? {
                return Ok(());
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = recheck.tick() => {}
            }
        }
    }

    /// Hand off pending work, one handoff at a time: wait for room on the
    /// stream, then in one database call claim it (committed as attempted)
    /// and queue it. The frame stream records it sent when the transport
    /// takes it. Waiting for room is cancellable; the call is short and
    /// always finishes. Returns false when the stream has gone.
    async fn hand_off(&self) -> Result<bool, Status> {
        loop {
            let Ok(permit) = self.tx.clone().reserve_owned().await else {
                return Ok(false);
            };
            let id = self.id.clone();
            let claimed = call(&self.state, move |conn| {
                let Some((outbox, offer)) = integration::claim(conn, &id)? else {
                    return Ok(false);
                };
                let handoff = offer.handoff_id.clone();
                let tx = permit.send(Item {
                    frame: frame(Out::Outgoing(offer)),
                    claim: Some(outbox),
                });
                if tx.is_closed() {
                    // The queue closed: abandoned, or about to be.
                    integration::mark_unknown(conn, &[outbox], NOT_HANDED_OFF)?;
                }
                tracing::info!(integration = id, handoff, "queued handoff");
                Ok(true)
            })
            .await?;
            if !claimed {
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests;
