//! Hub's integration stream (`bbs.integration.v1`) on its own listener,
//! `bbs serve --integration-listen`. The adapter connects with an integration
//! credential, sends its hello, and the stream carries both directions: BBS
//! publishes its catalog and hands off outbound work; the adapter offers
//! incoming messages, reports results and asks directory queries. Rules and
//! storage are in `service::integration`; this runs the stream.
//!
//! One stream per integration: a new connection supersedes the older one,
//! which ends. Outbound work is claimed (marked attempted and committed)
//! before it is queued for the stream (D072). A claim becomes `sent` when the
//! transport takes its frame; one still queued when its stream closes
//! provably never left, and returns to pending. The queue's lock makes those
//! two outcomes exclusive: see [`Outbound`].

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
use bbs_core::hub_proto::msgproto_v1 as mp;
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

/// The most remote directory queries waiting on one stream, and the most
/// of them one caller may have. Each sends one frame, on the stream's
/// outbound queue with handoffs: a handoff waits behind at most the first
/// bound of query frames (the queue's senders are served in turn), and a
/// query waits for room only until its deadline.
const MAX_REMOTE_QUERIES: usize = 128;
const MAX_REMOTE_QUERIES_PER_CALLER: usize = 8;
/// How long a remote directory query waits for the adapter's result: the
/// deadline the query carries, and a margin for the way back.
const REMOTE_WAIT: Duration = Duration::from_secs(6);

/// Open streams by integration ID.
pub(super) type Sessions = Arc<Mutex<HashMap<String, Arc<Link>>>>;

/// Remote directory queries waiting for their results, by query ID.
type Waiting = HashMap<String, Query>;

/// A remote directory query waiting for its result.
struct Query {
    /// Who asked, as its requester entity ID.
    caller: String,
    done: oneshot::Sender<Result<mp::DirectoryResult, Status>>,
}

/// An open stream, as the server reaches it.
pub(super) struct Link {
    /// Set to end the stream for a newer one.
    superseded: watch::Sender<bool>,
    /// Weak, so the stream still ends when its session lets go of it.
    tx: mpsc::WeakSender<Item>,
    /// None once the stream has ended: no result can come.
    queries: Mutex<Option<Waiting>>,
}

impl Link {
    fn queries(&self) -> std::sync::MutexGuard<'_, Option<Waiting>> {
        self.queries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stream has ended: its waiting queries fail at once.
    fn close(&self) {
        self.queries().take();
    }

    /// The adapter's result for a remote directory query, or why it was
    /// malformed; one nothing waits for (late, or never asked) is dropped.
    fn answered(&self, query_id: &str, result: Result<mp::DirectoryResult, Status>) {
        let waiter = self.queries().as_mut().and_then(|q| q.remove(query_id));
        match waiter {
            Some(w) => {
                let _ = w.done.send(result);
            }
            None => tracing::debug!(
                query = query_id,
                "a late or unknown remote directory result"
            ),
        }
    }
}

/// A query's place among its stream's waiting queries, given up when the
/// asker stops waiting, however that happens.
struct Waiter<'a> {
    link: &'a Link,
    id: String,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        if let Some(q) = self.link.queries().as_mut() {
            q.remove(&self.id);
        }
    }
}

/// Ask `integration`'s adapter a remote directory query, and wait up to
/// [`REMOTE_WAIT`] for its result. A result with an error code is returned
/// as is; failing to get one at all is `Unavailable`.
pub(super) async fn remote_directory(
    state: &State,
    integration: &str,
    query: mp::DirectoryQuery,
) -> Result<mp::DirectoryResult, Status> {
    let deadline = tokio::time::Instant::now() + REMOTE_WAIT;
    let link = state
        .integrations
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(integration)
        .cloned()
        .ok_or_else(|| {
            Status::unavailable(format!(
                "relay_unavailable: integration {integration} is not connected; is its \
                 adapter running?"
            ))
        })?;
    let caller = query
        .requester
        .as_ref()
        .and_then(|r| r.entity.as_ref())
        .map(|e| e.entity_id.clone())
        .unwrap_or_default();
    let (tx, rx) = oneshot::channel();
    let waiter = {
        let mut queries = link.queries();
        let Some(waiting) = queries.as_mut() else {
            return Err(Status::unavailable(format!(
                "relay_unavailable: integration {integration}'s stream has ended"
            )));
        };
        if waiting.len() >= MAX_REMOTE_QUERIES {
            return Err(Status::resource_exhausted(format!(
                "queue_full: too many remote directory queries are waiting on integration \
                 {integration}"
            )));
        }
        let mine = waiting.values().filter(|q| q.caller == caller).count();
        if mine >= MAX_REMOTE_QUERIES_PER_CALLER {
            return Err(Status::resource_exhausted(format!(
                "queue_full: you have {mine} remote directory queries waiting; wait for \
                 one to finish"
            )));
        }
        waiting.insert(query.query_id.clone(), Query { caller, done: tx });
        Waiter {
            link: &link,
            id: query.query_id.clone(),
        }
    };
    let ended = || {
        Status::unavailable(format!(
            "relay_unavailable: integration {integration}'s stream ended before it answered"
        ))
    };
    let timed_out = || {
        Status::deadline_exceeded(format!(
            "directory_timeout: integration {integration} did not answer in time"
        ))
    };
    let item = Item {
        frame: frame(Out::RemoteDirectoryQuery(query)),
        claim: None,
    };
    let Some(tx) = link.tx.upgrade() else {
        return Err(ended());
    };
    match tokio::time::timeout_at(deadline, tx.send(item)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err(ended()),
        Err(_) => return Err(timed_out()),
    }
    let result = match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err(ended()),
        Err(_) => return Err(timed_out()),
    };
    drop(waiter);
    Ok(result)
}

/// The client's status for a remote directory result's error: Hub's reason
/// code first, then its detail.
pub(super) fn remote_status(namespace: &str, result: &mp::DirectoryResult) -> Status {
    let code: String = result
        .error_code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect();
    let detail: String = result
        .detail
        .chars()
        .take(1024)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let message = if detail.trim().is_empty() {
        format!("{code}: the directory of {namespace} is unavailable")
    } else {
        format!("{code}: {}", detail.trim())
    };
    match code.as_str() {
        "namespace_unknown" | "entity_unknown" => Status::not_found(message),
        "invalid_argument" | "invalid_cursor" | "invalid_address" => {
            Status::invalid_argument(message)
        }
        // Refused by the destination, or by this installation's adapter
        // verifying the answer.
        "permission_denied" | "route_not_authorized" | "invalid_signature" => {
            Status::permission_denied(message)
        }
        "expired" | "directory_timeout" => Status::deadline_exceeded(message),
        "queue_full" => Status::resource_exhausted(message),
        // A remote home cannot judge the local caller's credentials (Hub's
        // herr.Remote): its claims about a token or scope, scope_denied
        // included, are UNAVAILABLE, as is everything else.
        "invalid_token" | "token_revoked" | "token_unknown" | "scope_denied" => {
            Status::unavailable(message)
        }
        _ => Status::unavailable(message),
    }
}

/// A frame for the adapter, with the outbox row it hands off, if any.
struct Item {
    frame: pb::BbsFrame,
    claim: Option<i64>,
}

/// The frames waiting for the transport. Every move of a claim across the
/// boundary happens under this lock: queuing one (only while open), taking
/// one for the transport (only while open), and closing (which drains what
/// is left). So a claim is either taken, and recorded sent, or drained, and
/// returned to pending; never both, and nothing queued after the close.
struct Outbound {
    rx: mpsc::Receiver<Item>,
    closed: bool,
}

type Queue = Arc<Mutex<Outbound>>;

fn lock(queue: &Queue) -> std::sync::MutexGuard<'_, Outbound> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(super) struct Integration {
    state: Arc<State>,
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

/// Record outbox rows in the background, as sent or back to pending: the
/// stream cannot wait for them.
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
        let conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let result = if sent {
            claims
                .iter()
                .try_for_each(|&id| integration::handed_off(&conn, id))
        } else {
            integration::requeue(&conn, &claims)
        };
        drop(conn);
        match result {
            // Requeued work is pending again: wake the next stream.
            Ok(()) if !sent => state.changed(),
            Ok(()) => {}
            Err(err) => tracing::error!("recording handoffs: {err:#}"),
        }
    });
}

/// Close the queue and drain it: none of what it holds reaches the
/// transport, so its claims return to pending.
fn abandon(queue: &Queue, state: &Arc<State>) {
    record(state, close(queue), false);
}

/// Close the queue, once; returns the claims it still held.
fn close(queue: &Queue) -> Vec<i64> {
    let mut out = lock(queue);
    if out.closed {
        return Vec::new();
    }
    out.closed = true;
    out.rx.close();
    let mut claims = Vec::new();
    while let Ok(item) = out.rx.try_recv() {
        claims.extend(item.claim);
    }
    claims
}

/// Queue `item` in the room `permit` reserved, unless the queue has closed,
/// in which case it is handed back: it was never queued.
fn enqueue(queue: &Queue, permit: mpsc::OwnedPermit<Item>, item: Item) -> Result<(), Item> {
    let out = lock(queue);
    if out.closed {
        return Err(item);
    }
    permit.send(item);
    Ok(())
}

/// Take the next frame for the transport; `Ready(None)` once the queue is
/// closed or every sender has gone.
fn take(queue: &Queue, cx: &mut std::task::Context<'_>) -> Poll<Option<Item>> {
    let mut out = lock(queue);
    if out.closed {
        return Poll::Ready(None);
    }
    out.rx.poll_recv(cx)
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
        match take(&this.queue, cx) {
            Poll::Ready(Some(item)) => {
                record(&this.state, item.claim.into_iter().collect(), true);
                Poll::Ready(Some(Ok(item.frame)))
            }
            // Closed with a terminal status on its way: wait for it, so the
            // stream never ends with OK in its place. The session sends it
            // before closing, and the receiver above is registered.
            Poll::Ready(None) if this.terminal.is_some() && lock(&this.queue).closed => {
                Poll::Pending
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
    fn supersede(&self, id: &str, tx: &mpsc::Sender<Item>) -> Arc<Link> {
        let mine = Arc::new(Link {
            superseded: watch::Sender::new(false),
            tx: tx.downgrade(),
            queries: Mutex::new(Some(HashMap::new())),
        });
        let mut sessions = self
            .state
            .integrations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = sessions.insert(id.to_string(), mine.clone()) {
            old.superseded.send_replace(true);
        }
        mine
    }

    fn release(&self, id: &str, mine: &Arc<Link>) {
        mine.close();
        let mut sessions = self
            .state
            .integrations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
        let (tx, rx) = mpsc::channel(OUTBOUND_FRAMES);
        // Room for the hello is certain: the channel is new, and nothing
        // else can reach it before it is registered below.
        let _ = tx.try_send(Item {
            frame: frame(Out::Hello(bbs_hello)),
            claim: None,
        });
        let link = self.supersede(&id, &tx);
        tracing::info!(
            integration = id,
            build = hello.build,
            ?peer,
            "integration connected"
        );
        let queue: Queue = Arc::new(Mutex::new(Outbound { rx, closed: false }));
        let (terminal, terminal_rx) = oneshot::channel();
        let this = self.clone();
        let session_queue = queue.clone();
        tokio::spawn(async move {
            let mut ended = link.superseded.subscribe();
            let session = Session {
                state: state.clone(),
                id: id.clone(),
                tx,
                queue: session_queue.clone(),
                link: link.clone(),
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
                    // The status first, so the stream ends with it, then
                    // whatever is still queued goes back to pending.
                    let _ = terminal.send(status);
                    abandon(&session_queue, &state);
                }
            }
            this.release(&id, &link);
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
    queue: Queue,
    link: Arc<Link>,
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
                // A malformed result fails only its own query, if it
                // names one; the stream and its handoffs carry on.
                Some(In::RemoteDirectoryResult(result)) => {
                    let query_id = result.query_id.clone();
                    match integration::check_remote_directory(&result) {
                        Ok(()) => self.link.answered(&query_id, Ok(result)),
                        Err(why) => {
                            tracing::warn!(integration = id, "{why}");
                            self.link.answered(
                                &query_id,
                                Err(Status::unavailable(format!(
                                    "invalid_message: integration {id}'s adapter sent a \
                                     malformed answer: {why}"
                                ))),
                            );
                        }
                    }
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
            let queue = self.queue.clone();
            let (queued, open, failed) = call(&self.state, move |conn| {
                let claim = integration::claim(conn, &id)?;
                let Some((outbox, offer)) = claim.handoff else {
                    return Ok((false, true, claim.failed));
                };
                let handoff = offer.handoff_id.clone();
                let item = Item {
                    frame: frame(Out::Outgoing(offer)),
                    claim: Some(outbox),
                };
                if enqueue(&queue, permit, item).is_err() {
                    // Closed since the room was reserved: never queued.
                    integration::requeue(conn, &[outbox])?;
                    return Ok((false, false, claim.failed));
                }
                tracing::info!(integration = id, handoff, "queued handoff");
                Ok((true, true, claim.failed))
            })
            .await?;
            // A rejection at the claim mailed its sender a notice.
            if failed > 0 {
                self.state.changed();
            }
            if !queued {
                return Ok(open);
            }
        }
    }
}

#[cfg(test)]
mod tests;
