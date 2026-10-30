//! `bbs serve`: a gRPC server over the service layer. It owns the SQLite file;
//! message writers must use this process so waiters see their commits. Local
//! credential administration is observed separately on calls and stream checks.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::db;
use crate::proto::bbs_server::{Bbs, BbsServer};
use crate::proto::{self, Kind};
use crate::protocol::Policy;
use crate::service::{self, Caller, TimerLimits, Watched};

mod web;

/// Keepalive pings let a vanished client free its waiter slot within ~30s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// A waiting agent is alive; keep it clear of stale-agent retirement.
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Leave time for process teardown within a ten-second container grace period.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
/// Activity events stored together: at most this many, waiting at most this
/// long for more after the first.
const ACTIVITY_BATCH: usize = 64;
const ACTIVITY_BATCH_WAIT: Duration = Duration::from_millis(50);

/// `bbs serve --web`: where the web UI listens, and the origin browsers reach
/// it at when that is not the listening address itself.
pub struct Web {
    pub addr: SocketAddr,
    pub origin: Option<String>,
}

pub async fn run(
    listen: SocketAddr,
    web: Option<Web>,
    dir: &Path,
    require_human_token: bool,
    protocol: Policy,
    timer_limits: TimerLimits,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(&dir.join("bbs.db"))?;
    db::migrate(&mut conn)?;
    serve(
        conn,
        listen,
        web,
        dir,
        require_human_token,
        protocol,
        timer_limits,
    )
    .await
}

async fn serve(
    conn: Connection,
    listen: SocketAddr,
    web: Option<Web>,
    dir: &Path,
    require_human_token: bool,
    protocol: Policy,
    timer_limits: TimerLimits,
) -> Result<()> {
    let signal = shutdown_signal()?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = listener.local_addr()?;
    // Bound and checked before anything is served, so a bad origin or a busy
    // port stops `serve` at once.
    let web = match web {
        Some(web) => {
            let listener = tokio::net::TcpListener::bind(web.addr)
                .await
                .with_context(|| format!("binding {}", web.addr))?;
            let boundary = web::Boundary::new(listener.local_addr()?, web.origin.as_deref())?;
            Some((listener, boundary))
        }
        None => None,
    };
    let state = Arc::new(State {
        conn: Mutex::new(conn),
        notify: watch::Sender::new(0),
        activity: watch::Sender::new(0),
        waiters: Mutex::new(HashSet::new()),
        shutdown: watch::Sender::new(false),
        require_human_token,
        protocol,
        auth_failures: Mutex::new(HashMap::new()),
        timer_limits,
        clock: Arc::new(Utc::now),
    });
    tokio::spawn(run_timers(state.clone()));
    let (health, health_service) = tonic_health::server::health_reporter();
    health.set_serving::<BbsServer<Server>>().await;
    let server = tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(health_service)
        .add_service(BbsServer::new(Server(state.clone())))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), state.stopping());
    tokio::pin!(server);
    // Tests read this line to learn the port when listening on port 0.
    println!("bbs serve listening on {addr}");
    // The web UI runs alongside the gRPC server, never apart from it: if it
    // fails, `serve` exits with its error rather than carry on without it.
    type WebUi = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
    let mut web_ui: Option<WebUi> = match web {
        Some((listener, boundary)) => {
            let web_addr = listener.local_addr()?;
            println!("bbs serve web UI on http://{web_addr}");
            tracing::info!(%web_addr, origin = %boundary.origin(), "serving web UI");
            Some(Box::pin(web::serve(
                listener,
                Server(state.clone()),
                boundary,
            )))
        }
        None => None,
    };
    tracing::info!(%addr, state_dir = %dir.display(), "serving");
    tokio::select! {
        result = &mut server => result?,
        result = async {
            match web_ui.as_mut() {
                Some(web_ui) => web_ui.await,
                None => std::future::pending().await,
            }
        } => {
            // It only returns once shutdown begins, which has not.
            result?;
            anyhow::bail!("the web UI stopped serving");
        }
        result = signal => {
            result?;
            health.set_not_serving::<BbsServer<Server>>().await;
            health.set_service_status("", tonic_health::ServingStatus::NotServing).await;
            state.shutdown.send_replace(true);
            let drained = async {
                let web = async {
                    match web_ui.as_mut() {
                        Some(web_ui) => web_ui.await,
                        None => Ok(()),
                    }
                };
                let (grpc, web) = tokio::join!(&mut server, web);
                grpc?;
                web
            };
            match tokio::time::timeout(SHUTDOWN_GRACE, drained).await {
                Ok(result) => result?,
                // Health watchers and clients that stop consuming streams can
                // otherwise keep the HTTP/2 connection alive indefinitely.
                Err(_) => tracing::warn!("shutdown grace expired; closing remaining connections"),
            }
        }
    }
    Ok(())
}

fn shutdown_signal() -> Result<impl std::future::Future<Output = Result<()>>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            Ok(())
        })
    }
    #[cfg(not(unix))]
    Ok(async { tokio::signal::ctrl_c().await.map_err(Into::into) })
}

struct State {
    conn: Mutex<Connection>,
    /// Bumped after every committed change; waiters and feeds re-check on it.
    notify: watch::Sender<u64>,
    /// Bumped after each batch of agent activity, which nothing else watches.
    activity: watch::Sender<u64>,
    /// Inboxes with a `WaitForMail` stream open, as (kind, id).
    waiters: Mutex<HashSet<(i32, i64)>>,
    shutdown: watch::Sender<bool>,
    require_human_token: bool,
    protocol: Policy,
    auth_failures: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    timer_limits: TimerLimits,
    /// The time timers fire by; tests set it.
    clock: Clock,
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// What a watch stream waits on between snapshots: one change signal, or
/// two, and optionally a shortest time between fetches, so a busy board
/// costs a watcher at most one fetch per interval.
struct Changes {
    signal: watch::Receiver<u64>,
    also: Option<watch::Receiver<u64>>,
    interval: Option<(Duration, tokio::time::Instant)>,
}

impl Changes {
    fn new(signal: watch::Receiver<u64>) -> Changes {
        Changes {
            signal,
            also: None,
            interval: None,
        }
    }

    /// Wake on either signal, at most once per `interval`.
    fn coalesced(
        signal: watch::Receiver<u64>,
        also: watch::Receiver<u64>,
        interval: Duration,
    ) -> Changes {
        Changes {
            signal,
            also: Some(also),
            interval: Some((interval, tokio::time::Instant::now())),
        }
    }

    /// Until a change, and then until the interval since the last fetch is up.
    async fn changed(&mut self) {
        // A signal's sender lives as long as the server state, so neither ends.
        match &mut self.also {
            Some(also) => {
                tokio::select! {
                    _ = self.signal.changed() => {}
                    _ = also.changed() => {}
                }
            }
            None => {
                let _ = self.signal.changed().await;
            }
        }
        if let Some((interval, last)) = self.interval {
            tokio::time::sleep_until(last + interval).await;
        }
    }

    /// About to fetch: what has changed so far is covered.
    fn taken(&mut self) {
        self.signal.borrow_and_update();
        if let Some(also) = &mut self.also {
            also.borrow_and_update();
        }
        if let Some((_, last)) = &mut self.interval {
            *last = tokio::time::Instant::now();
        }
    }
}

/// The shortest time between a summary watcher's fetches.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

/// The longest the scheduler sleeps, so a changed wall clock is noticed.
const TIMER_RECHECK: Duration = Duration::from_secs(60);

/// Fire due timers, then sleep until the next is due (or the recheck), waking
/// early after any committed change, which may have added one. A timer due
/// while the server was down fires on the first pass.
async fn run_timers(state: Arc<State>) {
    let mut changes = state.notify.subscribe();
    loop {
        changes.borrow_and_update();
        let now = (state.clock)();
        let wait = match state
            .call(move |conn| {
                let completed = service::check_agent_drains(conn, now)?;
                let (fired, next) = service::fire_timers(conn, now)?;
                Ok((fired + completed, next))
            })
            .await
        {
            Ok((fired, next)) => {
                if fired > 0 {
                    state.changed();
                }
                match next {
                    // Already due (more than one pass's worth): go again now.
                    Some(next) => (next - now)
                        .to_std()
                        .unwrap_or(Duration::ZERO)
                        .min(TIMER_RECHECK),
                    None => TIMER_RECHECK,
                }
            }
            Err(status) if status.code() == tonic::Code::Unavailable => return,
            Err(status) => {
                tracing::warn!("firing timers: {}", status.message());
                TIMER_RECHECK
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = changes.changed() => {}
            _ = state.stopping() => return,
        }
    }
}

impl State {
    fn check_auth_rate(&self, peer: Option<IpAddr>) -> Result<(), Status> {
        let mut failures = self
            .auth_failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        failures.retain(|_, (since, _)| since.elapsed() < Duration::from_secs(60));
        if peer.is_some_and(|ip| failures.get(&ip).is_some_and(|(_, count)| *count >= 20))
            || (failures.len() >= 1024 && peer.is_some_and(|ip| !failures.contains_key(&ip)))
        {
            return Err(Status::resource_exhausted(
                "too many authentication failures; retry after one minute",
            ));
        }
        Ok(())
    }

    async fn enforce_auth_rate(
        self: &Arc<Self>,
        peer: Option<IpAddr>,
        caller: &Caller,
    ) -> Result<(), Status> {
        if let Err(limited) = self.check_auth_rate(peer) {
            // NAT, proxies and port-forwards can put many humans on one IP.
            // An invalid client must never lock out authenticated colleagues.
            let caller = caller.clone();
            self.call(move |conn| service::whoami(conn, &caller))
                .await
                .map_err(|_| limited)?;
        }
        Ok(())
    }

    fn record_auth_failure(&self, peer: Option<IpAddr>, status: &Status) {
        if status.code() != tonic::Code::Unauthenticated {
            return;
        }
        if let Some(peer) = peer {
            let mut failures = self
                .auth_failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if failures.len() < 1024 || failures.contains_key(&peer) {
                let entry = failures.entry(peer).or_insert((Instant::now(), 0));
                entry.1 = entry.1.saturating_add(1);
            }
        }
    }

    /// Runs independently of message delivery, including blocked channel sends.
    async fn watch_credentials(self: &Arc<Self>, caller: Arc<Caller>) -> Status {
        if !self.require_human_token && caller.human_token.is_none() {
            return std::future::pending().await;
        }
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let c = caller.clone();
            if let Err(status) = self.call(move |conn| service::whoami(conn, &c)).await {
                return status;
            }
        }
    }

    async fn stopping(&self) {
        let _ = self
            .shutdown
            .subscribe()
            .wait_for(|stopping| *stopping)
            .await;
    }

    /// Run `f` on the connection off the async runtime.
    async fn call<T, F>(self: &Arc<Self>, f: F) -> Result<T, Status>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
            if *state.shutdown.borrow() {
                return Err(Status::unavailable("server is shutting down"));
            }
            f(&mut conn).map_err(|err| {
                if err
                    .downcast_ref::<crate::auth::AuthenticationError>()
                    .is_some()
                {
                    Status::unauthenticated(err.to_string())
                } else if err.downcast_ref::<service::SyncPageTooLarge>().is_some() {
                    Status::resource_exhausted(err.to_string())
                } else {
                    Status::failed_precondition(format!("{err:#}"))
                }
            })
        })
        .await
        .map_err(|err| {
            tracing::error!("service call failed: {err}");
            Status::internal("internal error")
        })?
    }

    /// Log a failed RPC with the caller's name. Tokens are credentials, so an
    /// agent is logged by name, or as unknown if its token resolves to no one.
    async fn warn_failed(self: &Arc<Self>, rpc: &str, caller: &Caller, status: &Status) {
        let who = match (&caller.token, &caller.human) {
            (Some(token), _) => {
                let token = token.trim().to_ascii_lowercase();
                self.call(move |conn| db::agent_by_token(conn, &token))
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(|| "<unknown token>".to_string(), |agent| agent.name)
            }
            (None, Some(human)) => human.clone(),
            (None, None) => "-".to_string(),
        };
        tracing::warn!(rpc, caller = who, "rpc failed: {}", status.message());
    }

    fn changed(&self) {
        self.notify.send_modify(|v| *v += 1);
    }

    /// Wait for unread mail, then stream it as `Read` would take it.
    async fn wait(
        self: &Arc<Self>,
        caller: Arc<Caller>,
        me: proto::Principal,
        limit: Option<u32>,
        tx: &mpsc::Sender<Result<proto::Message, Status>>,
    ) -> Result<(), Status> {
        let mut changes = self.notify.subscribe();
        let mut touch =
            tokio::time::interval_at(Instant::now() + WAIT_TOUCH_INTERVAL, WAIT_TOUCH_INTERVAL);
        loop {
            // Mark seen before checking, so a commit after the check still wakes us.
            changes.borrow_and_update();
            if me.kind() == Kind::Agent {
                let id = me.id;
                if !self.call(move |c| db::agent_is_live(c, id)).await? {
                    let msg = format!("{} was retired while waiting", me.name);
                    return Err(Status::failed_precondition(msg));
                }
            }
            let c = caller.clone();
            let req = proto::ReadRequest {
                limit,
                ..Default::default()
            };
            let msgs = self.call(move |conn| service::read(conn, &c, req)).await?;
            if !msgs.messages.is_empty() {
                self.changed();
                for m in msgs.messages {
                    if tx.send(Ok(m)).await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = touch.tick() => {
                    let c = caller.clone();
                    self.call(move |conn| service::whoami(conn, &c)).await?;
                }
                _ = tx.closed() => return Ok(()),
            }
        }
    }
}

/// A claimed `WaitForMail` inbox, released when the stream ends or the client goes away.
struct Slot {
    state: Arc<State>,
    key: (i32, i64),
}

impl Slot {
    fn claim(state: &Arc<State>, me: &proto::Principal) -> Result<Slot, Status> {
        let key = (me.kind, me.id);
        let mut waiters = state.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        if !waiters.insert(key) {
            let msg = format!(
                "another `bbs read --wait` is already running for {}",
                me.name
            );
            return Err(Status::failed_precondition(msg));
        }
        Ok(Slot {
            state: state.clone(),
            key,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut waiters = self
            .state
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        waiters.remove(&self.key);
    }
}

#[derive(Clone)]
struct Server(Arc<State>);

fn caller<T>(req: &Request<T>, require_human_token: bool) -> Caller {
    let get = |key| {
        req.metadata()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    Caller {
        token: get("bbs-token"),
        human: get("bbs-human"),
        human_token: get("bbs-human-token"),
        require_human_token,
    }
}

/// Whether a successful call changed state that waiters or feeds can observe.
#[derive(Clone, Copy, PartialEq)]
enum Effect {
    Query,
    Write,
}

impl Server {
    async fn unary<Req, T, F>(
        &self,
        rpc: &str,
        req: Request<Req>,
        effect: Effect,
        f: F,
    ) -> Result<Response<T>, Status>
    where
        Req: Send + 'static,
        T: Send + 'static,
        F: FnOnce(&mut Connection, &Caller, Req) -> Result<T> + Send + 'static,
    {
        self.0.protocol.validate(req.metadata())?;
        let result = async {
            let peer = req.remote_addr().map(|addr| addr.ip());
            let caller = Arc::new(caller(&req, self.0.require_human_token));
            self.0.enforce_auth_rate(peer, &caller).await?;
            if self.0.require_human_token && matches!(rpc, "ensure_human" | "add_human") {
                return Err(Status::permission_denied(
                    "human accounts must be provisioned with local `bbs admin human` commands",
                ));
            }
            let req = req.into_inner();
            let c = caller.clone();
            let out = match self.0.call(move |conn| f(conn, &c, req)).await {
                Ok(out) => out,
                Err(status) => {
                    self.0.record_auth_failure(peer, &status);
                    self.0.warn_failed(rpc, &caller, &status).await;
                    return Err(status);
                }
            };
            if effect == Effect::Write {
                self.0.changed();
            }
            Ok(Response::new(out))
        }
        .await;
        self.0.protocol.finish(result)
    }
}

impl Server {
    /// Stream snapshots of `req`: the first at once (which also checks the
    /// caller, before the stream opens), then another after each change seen
    /// on `changes`. In cursor mode an empty batch is not sent after the first.
    async fn watch<R: Watched>(
        &self,
        rpc: &'static str,
        req: Request<R>,
        mut changes: Changes,
    ) -> Result<Response<BoxStream<R::Snapshot>>, Status> {
        self.0.protocol.validate(req.metadata())?;
        let result = async {
            let peer = req.remote_addr().map(|addr| addr.ip());
            let caller = Arc::new(caller(&req, self.0.require_human_token));
            self.0.enforce_auth_rate(peer, &caller).await?;
            let req = req.into_inner();
            let state = self.0.clone();
            let c = caller.clone();
            let r = req.clone();
            let first = match state.call(move |conn| R::fetch(conn, &c, r)).await {
                Ok(first) => first,
                Err(status) => {
                    state.record_auth_failure(peer, &status);
                    state.warn_failed(rpc, &caller, &status).await;
                    return Err(status);
                }
            };
            let (tx, rx) = mpsc::channel(4);
            let (terminal, terminal_rx) = oneshot::channel();
            tokio::spawn(async move {
                let send_snapshots = async {
                    let mut req = req;
                    let mut snapshot = Ok(first);
                    let mut sent = false;
                    let mut last: Option<R::Snapshot> = None;
                    loop {
                        if let Err(status) = &snapshot {
                            state.warn_failed(rpc, &caller, status).await;
                        }
                        let failed = snapshot.is_err();
                        // Cursor mode: move past this batch; a full one means
                        // more may be waiting, so fetch again without waiting.
                        let (drain, empty) = match &snapshot {
                            Ok(s) => (req.advance(s), R::is_empty(s)),
                            Err(_) => (false, false),
                        };
                        let repeat = R::ONLY_CHANGES
                            && last.is_some()
                            && snapshot.as_ref().ok() == last.as_ref();
                        let skip = (sent && empty && req.has_cursor()) || repeat;
                        if R::ONLY_CHANGES && !skip {
                            last = snapshot.as_ref().ok().cloned();
                        }
                        if !skip && (tx.send(snapshot).await.is_err() || failed) {
                            return;
                        }
                        sent = true;
                        if !drain {
                            tokio::select! {
                                _ = changes.changed() => {}
                                _ = tx.closed() => return,
                            }
                        }
                        changes.taken();
                        let (c, r) = (caller.clone(), req.clone());
                        snapshot = state.call(move |conn| R::fetch(conn, &c, r)).await;
                    }
                };
                tokio::select! {
                    _ = state.stopping() => {
                        let _ = terminal.send(Status::unavailable("server is shutting down"));
                    },
                    status = state.watch_credentials(caller.clone()) => {
                        state.record_auth_failure(peer, &status);
                        let _ = terminal.send(status);
                    },
                    _ = send_snapshots => {},
                }
            });
            Ok(Response::new(Box::pin(CancellableStream {
                messages: ReceiverStream::new(rx),
                terminal: Some(terminal_rx),
                done: false,
                protocol: self.0.protocol.clone(),
            }) as BoxStream<R::Snapshot>))
        }
        .await;
        self.0.protocol.finish(result)
    }
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Terminal errors bypass buffered data and never block a producer's cleanup.
struct CancellableStream<T> {
    messages: ReceiverStream<Result<T, Status>>,
    terminal: Option<oneshot::Receiver<Status>>,
    done: bool,
    protocol: Policy,
}

impl<T> Stream for CancellableStream<T> {
    type Item = Result<T, Status>;
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future;
        use std::task::Poll;
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(terminal) = &mut this.terminal {
            match Pin::new(terminal).poll(cx) {
                Poll::Ready(Ok(status)) => {
                    this.done = true;
                    this.messages.close();
                    return Poll::Ready(Some(Err(this.protocol.status(status))));
                }
                Poll::Ready(Err(_)) => this.terminal = None,
                Poll::Pending => {}
            }
        }
        Pin::new(&mut this.messages)
            .poll_next(cx)
            .map(|item| item.map(|result| result.map_err(|status| this.protocol.status(status))))
    }
}

#[tonic::async_trait]
impl Bbs for Server {
    async fn server_info(
        &self,
        _req: Request<proto::Empty>,
    ) -> Result<Response<proto::ServerInfoResponse>, Status> {
        self.0.protocol.finish(Ok(Response::new(
            self.0.protocol.info(self.0.require_human_token),
        )))
    }

    async fn ensure_human(
        &self,
        req: Request<proto::EnsureHumanRequest>,
    ) -> Result<Response<proto::EnsureHumanResponse>, Status> {
        self.unary("ensure_human", req, Effect::Write, |c, _, r| {
            service::ensure_human(c, r)
        })
        .await
    }

    async fn add_human(
        &self,
        req: Request<proto::AddHumanRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary("add_human", req, Effect::Write, service::add_human)
            .await
    }

    async fn register(
        &self,
        req: Request<proto::RegisterRequest>,
    ) -> Result<Response<proto::RegisterResponse>, Status> {
        self.unary("register", req, Effect::Write, service::register)
            .await
    }

    async fn whoami(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::Principal>, Status> {
        self.unary("whoami", req, Effect::Query, |c, caller, _| {
            service::whoami(c, caller)
        })
        .await
    }

    async fn set_status(
        &self,
        req: Request<proto::SetStatusRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary("set_status", req, Effect::Write, service::set_status)
            .await
    }

    async fn bye(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::ByeResponse>, Status> {
        self.unary("bye", req, Effect::Write, |c, caller, _| {
            service::bye(c, caller)
        })
        .await
    }

    async fn retire(
        &self,
        req: Request<proto::RetireRequest>,
    ) -> Result<Response<proto::ByeResponse>, Status> {
        self.unary("retire", req, Effect::Write, service::retire_agent)
            .await
    }

    async fn rotate_agent_token(
        &self,
        req: Request<proto::RetireRequest>,
    ) -> Result<Response<proto::RegisterResponse>, Status> {
        // Without required human tokens anyone can claim to be the owner, and a
        // new token would let them act as the agent.
        if !self.0.require_human_token {
            self.0.protocol.validate(req.metadata())?;
            return Err(Status::failed_precondition(
                "rotating agent tokens requires a server started with --require-human-token",
            ));
        }
        self.unary(
            "rotate_agent_token",
            req,
            Effect::Write,
            service::rotate_agent_token,
        )
        .await
    }

    async fn group_register(
        &self,
        req: Request<proto::GroupRegisterRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary(
            "group_register",
            req,
            Effect::Write,
            service::group_register,
        )
        .await
    }

    async fn group_delete(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary("group_delete", req, Effect::Write, service::group_delete)
            .await
    }

    async fn group_describe(
        &self,
        req: Request<proto::GroupDescribeRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary(
            "group_describe",
            req,
            Effect::Write,
            service::group_describe,
        )
        .await
    }

    async fn group_join(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupJoinResponse>, Status> {
        self.unary("group_join", req, Effect::Write, service::group_join)
            .await
    }

    async fn group_leave(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary("group_leave", req, Effect::Write, service::group_leave)
            .await
    }

    async fn group_remove(
        &self,
        req: Request<proto::GroupRemoveRequest>,
    ) -> Result<Response<proto::GroupRemoveResponse>, Status> {
        self.unary("group_remove", req, Effect::Write, service::group_remove)
            .await
    }

    async fn group_add(
        &self,
        req: Request<proto::GroupAddRequest>,
    ) -> Result<Response<proto::GroupAddResponse>, Status> {
        self.unary("group_add", req, Effect::Write, service::group_add)
            .await
    }

    async fn set_group_writer(
        &self,
        req: Request<proto::SetGroupWriterRequest>,
    ) -> Result<Response<proto::SetGroupWriterResponse>, Status> {
        self.unary(
            "set_group_writer",
            req,
            Effect::Write,
            service::set_group_writer,
        )
        .await
    }

    async fn group_list(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::GroupListResponse>, Status> {
        self.unary("group_list", req, Effect::Query, |c, caller, _| {
            service::group_list(c, caller)
        })
        .await
    }

    async fn send(
        &self,
        req: Request<proto::SendRequest>,
    ) -> Result<Response<proto::SendResponse>, Status> {
        self.unary("send", req, Effect::Write, service::send).await
    }

    async fn auto_reply(
        &self,
        req: Request<proto::AutoReplyRequest>,
    ) -> Result<Response<proto::AutoReplyResponse>, Status> {
        self.unary("auto_reply", req, Effect::Write, service::auto_reply)
            .await
    }

    // Side-channel writes mutate no mail/feed/summary/timer view, so they do
    // not wake those watchers; the side UI and drivers poll independently.
    async fn begin_side_session(
        &self,
        req: Request<proto::BeginSideSessionRequest>,
    ) -> Result<Response<proto::BeginSideSessionResponse>, Status> {
        self.unary(
            "begin_side_session",
            req,
            Effect::Query,
            service::begin_side_session,
        )
        .await
    }

    async fn report_context(
        &self,
        req: Request<proto::ReportContextRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary(
            "report_context",
            req,
            Effect::Query,
            service::report_context,
        )
        .await
    }

    async fn request_owner_input(
        &self,
        req: Request<proto::RequestOwnerInputRequest>,
    ) -> Result<Response<proto::RequestOwnerInputResponse>, Status> {
        self.unary(
            "request_owner_input",
            req,
            Effect::Query,
            service::request_owner_input,
        )
        .await
    }

    async fn answer_owner_input(
        &self,
        req: Request<proto::AnswerOwnerInputRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary(
            "answer_owner_input",
            req,
            Effect::Query,
            service::answer_owner_input,
        )
        .await
    }

    async fn close_owner_input(
        &self,
        req: Request<proto::CloseOwnerInputRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary(
            "close_owner_input",
            req,
            Effect::Query,
            service::close_owner_input,
        )
        .await
    }

    async fn side_report(
        &self,
        req: Request<proto::SideReportRequest>,
    ) -> Result<Response<proto::SideReportResponse>, Status> {
        self.unary("side_report", req, Effect::Query, service::side_report)
            .await
    }

    async fn ask_side(
        &self,
        req: Request<proto::AskSideRequest>,
    ) -> Result<Response<proto::AskSideResponse>, Status> {
        self.unary("ask_side", req, Effect::Query, service::ask_side)
            .await
    }

    async fn cancel_side(
        &self,
        req: Request<proto::CancelSideRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary("cancel_side", req, Effect::Query, service::cancel_side)
            .await
    }

    async fn claim_side(
        &self,
        req: Request<proto::ClaimSideRequest>,
    ) -> Result<Response<proto::ClaimSideResponse>, Status> {
        self.unary("claim_side", req, Effect::Query, service::claim_side)
            .await
    }

    async fn report_side(
        &self,
        req: Request<proto::ReportSideRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary("report_side", req, Effect::Query, service::report_side)
            .await
    }

    async fn read(
        &self,
        req: Request<proto::ReadRequest>,
    ) -> Result<Response<proto::ReadResponse>, Status> {
        let resp = self
            .unary("read", req, Effect::Query, service::read)
            .await?;
        // Taking mail sets read_at, which the feed shows.
        if !resp.get_ref().messages.is_empty() {
            self.0.changed();
        }
        Ok(resp)
    }

    type WaitForMailStream = BoxStream<proto::Message>;

    async fn wait_for_mail(
        &self,
        req: Request<proto::WaitRequest>,
    ) -> Result<Response<Self::WaitForMailStream>, Status> {
        self.0.protocol.validate(req.metadata())?;
        let result = async {
            let peer = req.remote_addr().map(|addr| addr.ip());
            let caller = caller(&req, self.0.require_human_token);
            self.0.enforce_auth_rate(peer, &caller).await?;
            let limit = req.into_inner().limit;
            let c = caller.clone();
            let claim = async {
                let me = self.0.call(move |conn| service::whoami(conn, &c)).await?;
                // Claim before returning, so a second waiter fails immediately.
                let slot = Slot::claim(&self.0, &me)?;
                Ok::<_, Status>((me, slot))
            };
            let (me, slot) = match claim.await {
                Ok(claimed) => claimed,
                Err(status) => {
                    self.0.record_auth_failure(peer, &status);
                    self.0.warn_failed("wait_for_mail", &caller, &status).await;
                    return Err(status);
                }
            };
            let (tx, rx) = mpsc::channel(16);
            let (terminal, terminal_rx) = oneshot::channel();
            let state = self.0.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let caller = Arc::new(caller);
                tokio::select! {
                    _ = state.stopping() => {
                        let _ = terminal.send(Status::unavailable("server is shutting down"));
                    },
                    status = state.watch_credentials(caller.clone()) => {
                        state.record_auth_failure(peer, &status);
                        let _ = terminal.send(status);
                    },
                    _ = async {
                        if let Err(status) = state.wait(caller.clone(), me, limit, &tx).await {
                            state.record_auth_failure(peer, &status);
                            state.warn_failed("wait_for_mail", &caller, &status).await;
                            let _ = tx.send(Err(status)).await;
                        }
                    } => {},
                }
            });
            Ok(Response::<Self::WaitForMailStream>::new(Box::pin(
                CancellableStream {
                    messages: ReceiverStream::new(rx),
                    terminal: Some(terminal_rx),
                    done: false,
                    protocol: self.0.protocol.clone(),
                },
            )))
        }
        .await;
        self.0.protocol.finish(result)
    }

    async fn list_blockers(
        &self,
        req: Request<proto::ListBlockersRequest>,
    ) -> Result<Response<proto::ListBlockersResponse>, Status> {
        self.unary("list_blockers", req, Effect::Query, service::list_blockers)
            .await
    }

    async fn get_blocker(
        &self,
        req: Request<proto::GetBlockerRequest>,
    ) -> Result<Response<proto::GetBlockerResponse>, Status> {
        self.unary("get_blocker", req, Effect::Query, service::get_blocker)
            .await
    }

    async fn list_agent_notes(
        &self,
        req: Request<proto::ListAgentNotesRequest>,
    ) -> Result<Response<proto::ListAgentNotesResponse>, Status> {
        self.unary(
            "list_agent_notes",
            req,
            Effect::Query,
            service::list_agent_notes,
        )
        .await
    }

    async fn put_agent_note(
        &self,
        req: Request<proto::PutAgentNoteRequest>,
    ) -> Result<Response<proto::PutAgentNoteResponse>, Status> {
        self.unary(
            "put_agent_note",
            req,
            Effect::Write,
            service::put_agent_note,
        )
        .await
    }

    async fn put_blocker(
        &self,
        req: Request<proto::PutBlockerRequest>,
    ) -> Result<Response<proto::BlockerMutationResponse>, Status> {
        self.unary("put_blocker", req, Effect::Write, service::put_blocker)
            .await
    }

    async fn answer_blocker(
        &self,
        req: Request<proto::AnswerBlockerRequest>,
    ) -> Result<Response<proto::BlockerMutationResponse>, Status> {
        self.unary(
            "answer_blocker",
            req,
            Effect::Write,
            service::answer_blocker,
        )
        .await
    }

    async fn cancel_blocker(
        &self,
        req: Request<proto::CancelBlockerRequest>,
    ) -> Result<Response<proto::BlockerMutationResponse>, Status> {
        self.unary(
            "cancel_blocker",
            req,
            Effect::Write,
            service::cancel_blocker,
        )
        .await
    }

    async fn search(
        &self,
        req: Request<proto::SearchRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        self.unary("search", req, Effect::Query, service::search)
            .await
    }

    async fn list_agents(
        &self,
        req: Request<proto::ListAgentsRequest>,
    ) -> Result<Response<proto::ListAgentsResponse>, Status> {
        self.unary("list_agents", req, Effect::Query, service::list_agents)
            .await
    }

    type WatchFeedStream = BoxStream<proto::FeedSnapshot>;

    async fn watch_feed(
        &self,
        req: Request<proto::FeedRequest>,
    ) -> Result<Response<Self::WatchFeedStream>, Status> {
        let changes = self.0.notify.subscribe();
        self.watch("watch_feed", req, Changes::new(changes)).await
    }

    async fn post_activity(
        &self,
        req: Request<Streaming<proto::ActivityEvent>>,
    ) -> Result<Response<proto::PostActivityResponse>, Status> {
        self.0.protocol.validate(req.metadata())?;
        let result = async {
            let peer = req.remote_addr().map(|addr| addr.ip());
            let caller = Arc::new(caller(&req, self.0.require_human_token));
            self.0.enforce_auth_rate(peer, &caller).await?;
            let state = self.0.clone();
            // Each batch is what arrived together, stored in one transaction.
            let batches = req
                .into_inner()
                .chunks_timeout(ACTIVITY_BATCH, ACTIVITY_BATCH_WAIT);
            tokio::pin!(batches);
            let mut accepted = 0;
            let posting = async {
                while let Some(batch) = batches.next().await {
                    let batch = batch.into_iter().collect::<Result<Vec<_>, Status>>()?;
                    let c = caller.clone();
                    let posted = state
                        .call(move |conn| service::post_activity(conn, &c, batch))
                        .await;
                    match posted {
                        Ok(n) => accepted += n,
                        Err(status) => {
                            state.record_auth_failure(peer, &status);
                            state.warn_failed("post_activity", &caller, &status).await;
                            return Err(status);
                        }
                    }
                    state.activity.send_modify(|v| *v += 1);
                }
                Ok(())
            };
            tokio::select! {
                posted = posting => posted?,
                _ = state.stopping() => {
                    return Err(Status::unavailable("server is shutting down"));
                }
                status = state.watch_credentials(caller.clone()) => {
                    state.record_auth_failure(peer, &status);
                    return Err(status);
                }
            }
            Ok(Response::new(proto::PostActivityResponse { accepted }))
        }
        .await;
        self.0.protocol.finish(result)
    }

    async fn watch_agent_summary(
        &self,
        req: Request<proto::AgentSummaryRequest>,
    ) -> Result<Response<Self::WatchAgentSummaryStream>, Status> {
        let changes = Changes::coalesced(
            self.0.notify.subscribe(),
            self.0.activity.subscribe(),
            SUMMARY_INTERVAL,
        );
        self.watch("watch_agent_summary", req, changes).await
    }

    async fn mark_seen(
        &self,
        req: Request<proto::MarkSeenRequest>,
    ) -> Result<Response<proto::MarkSeenResponse>, Status> {
        // As with mark_read, only a moved cursor wakes summary watchers.
        let response = self
            .unary("mark_seen", req, Effect::Query, service::mark_seen)
            .await?;
        if response.get_ref().moved {
            self.0.changed();
        }
        Ok(response)
    }

    async fn mark_read(
        &self,
        req: Request<proto::MarkReadRequest>,
    ) -> Result<Response<proto::MarkReadResponse>, Status> {
        // Only a change wakes watchers: marking nothing must not refresh the
        // feeds whose snapshots prompt connect to mark.
        let response = self
            .unary("mark_read", req, Effect::Query, service::mark_read)
            .await?;
        if response.get_ref().count > 0 {
            self.0.changed();
        }
        Ok(response)
    }

    async fn agent_drain(
        &self,
        req: Request<proto::AgentDrainRequest>,
    ) -> Result<Response<proto::AgentDrainResponse>, Status> {
        let response = self
            .unary(
                "agent_drain",
                req,
                Effect::Query,
                service::agent_drain_with_changes,
            )
            .await?;
        if response.get_ref().1 {
            self.0.changed();
        }
        Ok(response.map(|(response, _)| response))
    }

    async fn report_drain(
        &self,
        req: Request<proto::ReportDrainRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        let response = self
            .unary(
                "report_drain",
                req,
                Effect::Query,
                service::report_drain_with_changes,
            )
            .await?;
        if response.get_ref().1 {
            self.0.changed();
        }
        Ok(response.map(|(response, _)| response))
    }

    async fn report_effort(
        &self,
        req: Request<proto::ReportEffortRequest>,
    ) -> Result<Response<proto::ReportEffortResponse>, Status> {
        self.unary("report_effort", req, Effect::Write, service::report_effort)
            .await
    }

    async fn timer_add(
        &self,
        req: Request<proto::TimerAddRequest>,
    ) -> Result<Response<proto::Timer>, Status> {
        let limits = self.0.timer_limits;
        self.unary("timer_add", req, Effect::Write, move |conn, caller, req| {
            service::timer_add(conn, caller, req, limits)
        })
        .await
    }

    async fn timer_list(
        &self,
        req: Request<proto::TimerListRequest>,
    ) -> Result<Response<proto::TimerListResponse>, Status> {
        self.unary("timer_list", req, Effect::Query, service::timer_list)
            .await
    }

    async fn timer_remove(
        &self,
        req: Request<proto::TimerRemoveRequest>,
    ) -> Result<Response<proto::Timer>, Status> {
        self.unary("timer_remove", req, Effect::Write, service::timer_remove)
            .await
    }

    type WatchAgentSummaryStream = BoxStream<proto::AgentSummarySnapshot>;
    type WatchActivityStream = BoxStream<proto::ActivitySnapshot>;

    async fn watch_activity(
        &self,
        req: Request<proto::ActivityRequest>,
    ) -> Result<Response<Self::WatchActivityStream>, Status> {
        let changes = self.0.activity.subscribe();
        self.watch("watch_activity", req, Changes::new(changes))
            .await
    }

    async fn bind_session(
        &self,
        req: Request<proto::BindSessionRequest>,
    ) -> Result<Response<proto::BindSessionResponse>, Status> {
        self.unary("bind_session", req, Effect::Write, |c, caller, r| {
            service::bind_session(c, caller, r)
        })
        .await
    }

    async fn session_agent(
        &self,
        req: Request<proto::SessionAgentRequest>,
    ) -> Result<Response<proto::SessionAgentResponse>, Status> {
        self.unary("session_agent", req, Effect::Query, |c, caller, r| {
            service::session_agent(c, caller, r)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DrainRig {
        server: Server,
        token: String,
    }

    impl DrainRig {
        fn new() -> Self {
            let mut conn = Connection::open_in_memory().unwrap();
            db::migrate(&mut conn).unwrap();
            service::ensure_human(
                &mut conn,
                proto::EnsureHumanRequest {
                    name: "will".into(),
                },
            )
            .unwrap();
            let owner = Caller {
                human: Some("will".into()),
                ..Default::default()
            };
            let token = service::register(
                &mut conn,
                &owner,
                proto::RegisterRequest {
                    name: Some("Mateo".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token;
            let state = Arc::new(State {
                conn: Mutex::new(conn),
                notify: watch::Sender::new(0),
                activity: watch::Sender::new(0),
                waiters: Mutex::new(HashSet::new()),
                shutdown: watch::Sender::new(false),
                require_human_token: false,
                protocol: Policy::default(),
                auth_failures: Mutex::new(HashMap::new()),
                timer_limits: TimerLimits::default(),
                clock: Arc::new(Utc::now),
            });
            Self {
                server: Server(state),
                token,
            }
        }

        async fn status(&self, action: &str) -> proto::AgentDrainResponse {
            self.server
                .agent_drain(with_metadata(
                    proto::AgentDrainRequest {
                        action: action.into(),
                    },
                    "bbs-human",
                    "will",
                ))
                .await
                .unwrap()
                .into_inner()
        }

        async fn report(&self, completed_through: i64, side_completed_through: i64) {
            self.server
                .report_drain(with_metadata(
                    proto::ReportDrainRequest {
                        completed_through,
                        side_completed_through,
                    },
                    "bbs-token",
                    &self.token,
                ))
                .await
                .unwrap();
        }

        fn notifications(&self) -> u64 {
            *self.server.0.notify.borrow()
        }

        fn read_notice(&self) {
            service::read(
                &mut self.server.0.conn.lock().unwrap(),
                &Caller {
                    token: Some(self.token.clone()),
                    ..Default::default()
                },
                proto::ReadRequest::default(),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn drain_rpcs_notify_only_for_domain_changes() {
        let rig = DrainRig::new();
        {
            let conn = rig.server.0.conn.lock().unwrap();
            conn.execute(
                "UPDATE humans SET active_at = '2001-01-01T00:00:00.000000Z'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE agents SET active_at = '2001-01-01T00:00:00.000000Z'",
                [],
            )
            .unwrap();
        }
        let before = rig.server.0.conn.lock().unwrap().total_changes();
        assert_eq!(rig.status("").await.status, "running");
        assert!(
            rig.server.0.conn.lock().unwrap().total_changes() > before,
            "authentication refreshed activity"
        );
        assert_eq!(
            rig.notifications(),
            0,
            "identity activity is not a drain state change"
        );
        rig.status("").await;
        rig.report(1, 0).await;
        assert_eq!(
            rig.notifications(),
            0,
            "no-op status and unmatched report stay quiet"
        );

        let started = rig.status("start").await;
        assert_eq!(rig.notifications(), 1);
        rig.status("start").await;
        rig.status("").await;
        rig.report(0, 0).await;
        assert_eq!(
            rig.notifications(),
            1,
            "duplicate start, query and stale report stay quiet"
        );
        rig.read_notice();
        rig.report(started.watermark, 0).await;
        assert_eq!(rig.notifications(), 2);
        assert_eq!(rig.status("").await.status, "ready");
        rig.report(started.watermark, 0).await;
        assert_eq!(rig.notifications(), 2, "duplicate completion stays quiet");
        rig.status("resume").await;
        assert_eq!(rig.notifications(), 3);
        rig.status("resume").await;
        assert_eq!(rig.notifications(), 3, "duplicate resume stays quiet");
    }

    #[tokio::test]
    async fn drain_status_query_notifies_when_reconciliation_creates_a_timeout_message() {
        let rig = DrainRig::new();
        rig.status("start").await;
        rig.server
            .0
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE agent_drains SET deadline = '2001-01-01T00:00:00.000000Z'",
                [],
            )
            .unwrap();
        assert_eq!(rig.status("").await.status, "timed_out");
        assert_eq!(rig.notifications(), 2);
        rig.status("").await;
        assert_eq!(rig.notifications(), 2);
        let messages: i64 = rig
            .server
            .0
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM deliveries WHERE recipient_kind = 'human'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn cancelled_side_turn_needs_runner_ack_and_only_new_progress_notifies() {
        let rig = DrainRig::new();
        let side_id = {
            let conn = rig.server.0.conn.lock().unwrap();
            conn.execute("INSERT INTO side_requests(agent_id, question, asked_at, status, updated_at) SELECT id, 'active side turn', 'now', 'open', 'now' FROM agents", []).unwrap();
            conn.last_insert_rowid()
        };
        let started = rig.status("start").await;
        rig.read_notice();
        rig.report(started.watermark, 0).await;
        assert_eq!(rig.notifications(), 2);
        let pending = rig.status("").await;
        assert!(pending.agents[0].completed);
        assert!(pending.agents[0].side_pending);
        assert_eq!(pending.status, "draining");
        {
            let mut conn = rig.server.0.conn.lock().unwrap();
            service::cancel_side(
                &mut conn,
                &Caller {
                    human: Some("will".into()),
                    ..Default::default()
                },
                proto::CancelSideRequest {
                    id: side_id,
                    end: false,
                },
            )
            .unwrap();
        }
        rig.report(started.watermark, 0).await;
        assert_eq!(rig.status("").await.status, "draining");
        assert_eq!(
            rig.notifications(),
            2,
            "owner cancellation and duplicate parent ack cannot confirm child quiescence"
        );
        rig.report(started.watermark, side_id).await;
        assert_eq!(rig.status("").await.status, "ready");
        assert_eq!(rig.notifications(), 3);
        rig.report(started.watermark, side_id).await;
        assert_eq!(rig.notifications(), 3);
    }

    #[tokio::test]
    async fn blockers_round_trip_and_old_capability_refusal_preserve_atomicity() {
        use crate::backend::{BLOCKER_CHOICES, BLOCKERS, BLOCKERS_MINE, Backend, Remote};
        for withheld in [
            Some(BLOCKERS),
            Some(BLOCKERS_MINE),
            Some(BLOCKER_CHOICES),
            None,
        ] {
            let supported = withheld.is_none();
            let mut conn = Connection::open_in_memory().unwrap();
            db::migrate(&mut conn).unwrap();
            service::ensure_human(
                &mut conn,
                proto::EnsureHumanRequest {
                    name: "will".into(),
                },
            )
            .unwrap();
            let will = Caller {
                human: Some("will".into()),
                ..Default::default()
            };
            service::group_register(
                &mut conn,
                &will,
                proto::GroupRegisterRequest {
                    group: "crew".into(),
                    description: None,
                },
            )
            .unwrap();
            let token = service::register(
                &mut conn,
                &will,
                proto::RegisterRequest {
                    name: Some("Mateo".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token;
            let mateo = Caller {
                token: Some(token),
                ..Default::default()
            };
            service::group_join(
                &mut conn,
                &mateo,
                proto::GroupRequest {
                    group: "crew".into(),
                },
            )
            .unwrap();
            let first = service::put_blocker(
                &mut conn,
                &mateo,
                proto::PutBlockerRequest {
                    group: "crew".into(),
                    body: "Question one".into(),
                    request_key: "one".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .blocker
            .unwrap()
            .id;
            let state = Arc::new(State {
                conn: Mutex::new(conn),
                notify: watch::Sender::new(0),
                activity: watch::Sender::new(0),
                waiters: Mutex::new(HashSet::new()),
                shutdown: watch::Sender::new(false),
                require_human_token: false,
                protocol: withheld
                    .map_or_else(Policy::default, |cap| Policy::default().without(cap)),
                auth_failures: Mutex::new(HashMap::new()),
                timer_limits: TimerLimits::default(),
                clock: Arc::new(Utc::now),
            });
            let snapshot = || {
                let conn = state.conn.lock().unwrap();
                ["blockers", "blocker_events", "messages", "deliveries"].map(|table| {
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap()
                })
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(BbsServer::new(Server(state.clone())))
                    .serve_with_incoming(TcpListenerStream::new(listener)),
            );
            let mut human = Backend::Remote(Remote::connect(&address, will).await.unwrap());
            let mut agent = Backend::Remote(Remote::connect(&address, mateo).await.unwrap());
            let put = proto::PutBlockerRequest {
                group: "crew".into(),
                body: "Question two".into(),
                request_key: "two".into(),
                ..Default::default()
            };
            let answer = proto::AnswerBlockerRequest {
                id: first,
                answer: "Proceed".into(),
                request_key: "answer".into(),
                ..Default::default()
            };
            let cancel = proto::CancelBlockerRequest {
                id: first,
                request_key: "cancel".into(),
            };
            if withheld == Some(BLOCKERS_MINE) {
                let before = snapshot();
                let error = human
                    .list_blockers(proto::ListBlockersRequest {
                        mine: true,
                        ..Default::default()
                    })
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.downcast_ref::<Status>().unwrap().code(),
                    tonic::Code::FailedPrecondition
                );
                assert!(format!("{error:#}").contains(BLOCKERS_MINE));
                assert_eq!(snapshot(), before);
                // A pre-extension server still supports ordinary blocker reads.
                assert_eq!(
                    human
                        .list_blockers(proto::ListBlockersRequest::default())
                        .await
                        .unwrap()
                        .active_count,
                    1
                );
                server.abort();
                continue;
            }
            if withheld == Some(BLOCKER_CHOICES) {
                let before = snapshot();
                let choices = proto::BlockerChoices {
                    mode: proto::BlockerChoiceMode::BlockerSelectOne.into(),
                    options: ["a", "b"]
                        .map(|id| proto::BlockerOption {
                            id: id.into(),
                            label: id.into(),
                            detail: None,
                        })
                        .into(),
                    ..Default::default()
                };
                let errors = [
                    agent
                        .put_blocker(proto::PutBlockerRequest {
                            choices: Some(choices),
                            ..put.clone()
                        })
                        .await
                        .unwrap_err(),
                    human
                        .answer_blocker(proto::AnswerBlockerRequest {
                            selected: vec!["a".into()],
                            ..answer.clone()
                        })
                        .await
                        .unwrap_err(),
                ];
                for error in errors {
                    assert_eq!(
                        error.downcast_ref::<Status>().unwrap().code(),
                        tonic::Code::FailedPrecondition
                    );
                    assert!(format!("{error:#}").contains(BLOCKER_CHOICES));
                }
                assert_eq!(snapshot(), before);
                // Text-only blockers still work against a pre-choices server.
                agent.put_blocker(put).await.unwrap();
                human.answer_blocker(answer).await.unwrap();
                server.abort();
                continue;
            }
            if !supported {
                let before = snapshot();
                let errors = [
                    agent.put_blocker(put).await.unwrap_err(),
                    human.answer_blocker(answer).await.unwrap_err(),
                    agent.cancel_blocker(cancel).await.unwrap_err(),
                    human
                        .list_blockers(proto::ListBlockersRequest::default())
                        .await
                        .unwrap_err(),
                    human
                        .get_blocker(proto::GetBlockerRequest {
                            id: first,
                            ..Default::default()
                        })
                        .await
                        .unwrap_err(),
                ];
                for error in errors {
                    assert!(format!("{error:#}").contains(BLOCKERS));
                    assert_eq!(
                        error.downcast_ref::<Status>().unwrap().code(),
                        tonic::Code::FailedPrecondition
                    );
                }
                assert_eq!(snapshot(), before);
            } else {
                let made = agent.put_blocker(put.clone()).await.unwrap();
                assert!(agent.put_blocker(put).await.unwrap().replayed);
                let list = human
                    .list_blockers(proto::ListBlockersRequest {
                        agent: Some("Mateo".into()),
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                assert_eq!(list.active_count, 2);
                assert_eq!(list.blockers[0].id, first);
                assert!(list.has_more);
                let response = human.answer_blocker(answer.clone()).await.unwrap();
                let replay = human.answer_blocker(answer).await.unwrap();
                assert_eq!(response.message_id, replay.message_id);
                assert!(replay.replayed);
                let get = human
                    .get_blocker(proto::GetBlockerRequest {
                        id: first,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    get.blocker.unwrap().status(),
                    proto::BlockerStatus::BlockerAnswered
                );
                agent
                    .cancel_blocker(proto::CancelBlockerRequest {
                        id: made.blocker.unwrap().id,
                        ..cancel
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    human
                        .list_blockers(proto::ListBlockersRequest::default())
                        .await
                        .unwrap()
                        .active_count,
                    0
                );
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn protocol_rejection_precedes_auth_budget_identity_lookup() {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let before = "2001-01-01T00:00:00.000000Z";
        conn.execute("UPDATE humans SET active_at = ?1", [before])
            .unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let state = Arc::new(State {
            conn: Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::from([(peer.ip(), (Instant::now(), 20))])),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let server = Server(state.clone());
        let request = || {
            let mut req = Request::new(proto::Empty {});
            req.metadata_mut()
                .insert("bbs-human", "will".parse().unwrap());
            req.extensions_mut()
                .insert(tonic::transport::server::TcpConnectInfo {
                    local_addr: None,
                    remote_addr: Some(peer),
                });
            req
        };
        assert_eq!(
            server.whoami(request()).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        let (metadata, extensions, _) = request().into_parts();
        assert!(
            server
                .wait_for_mail(Request::from_parts(
                    metadata.clone(),
                    extensions.clone(),
                    proto::WaitRequest { limit: None }
                ))
                .await
                .is_err()
        );
        assert!(
            server
                .watch_feed(Request::from_parts(
                    metadata,
                    extensions,
                    proto::FeedRequest {
                        backlog: 10,
                        ..Default::default()
                    }
                ))
                .await
                .is_err()
        );
        let after: String = state
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT active_at FROM humans WHERE name = 'will'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, before);
        assert!(state.waiters.lock().unwrap().is_empty());
    }

    /// Old-capability transport emulation: a server that does not advertise
    /// side-conversations.v1 refuses a follow-up, an end and an ENDED report
    /// on the required-capability header, before they touch any state, and
    /// one-shot side questions still work. Its service still understands
    /// the new fields, so without the header such a request would be
    /// accepted; this is not a schema-19 decoder.
    #[tokio::test]
    async fn an_older_server_refuses_side_conversations_before_any_write() {
        use crate::backend::{Backend, Remote, SIDE_CONVERSATIONS};
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let will = Caller {
            human: Some("will".into()),
            ..Default::default()
        };
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let token = service::register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let mateo = Caller {
            token: Some(token),
            ..Default::default()
        };
        service::begin_side_session(
            &mut conn,
            &mateo,
            proto::BeginSideSessionRequest {
                generation: "run-1".into(),
            },
        )
        .unwrap();
        // Even a runner claiming follow-ups cannot get past the transport.
        service::report_context(
            &mut conn,
            &mateo,
            proto::ReportContextRequest {
                generation: "run-1".into(),
                usage: None,
                side_supported: Some(true),
                side_reason: None,
                preserve_usage: false,
                side_followups: Some(true),
            },
        )
        .unwrap();
        let first = service::ask_side(
            &mut conn,
            &will,
            proto::AskSideRequest {
                agent: "Mateo".into(),
                question: "q1".into(),
                follows: None,
            },
        )
        .unwrap()
        .id;
        service::claim_side(
            &mut conn,
            &mateo,
            proto::ClaimSideRequest {
                generation: "run-1".into(),
            },
        )
        .unwrap();
        let done = proto::ReportSideRequest {
            generation: "run-1".into(),
            id: first,
            status: proto::SideStatus::SideCompleted.into(),
            answer: Some("a1".into()),
        };
        service::report_side(&mut conn, &mateo, done).unwrap();
        let state = Arc::new(State {
            conn: Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default().without(SIDE_CONVERSATIONS),
            auth_failures: Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(BbsServer::new(Server(state.clone())))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let snapshot = || -> Vec<String> {
            let conn = state.conn.lock().unwrap();
            let mut rows: Vec<String> = conn
                .prepare(
                    "SELECT id, status, answer, ended, follows, conversation, updated_at
                     FROM side_requests ORDER BY id",
                )
                .unwrap()
                .query_map([], |r| {
                    Ok(format!(
                        "{}|{}|{:?}|{}|{:?}|{}|{}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let revision: i64 = conn
                .query_row("SELECT revision FROM side_state", [], |r| r.get(0))
                .unwrap();
            rows.push(format!("revision {revision}"));
            rows
        };
        let mut owner = Backend::Remote(Remote::connect(&address, will).await.unwrap());
        let mut runner = Backend::Remote(Remote::connect(&address, mateo).await.unwrap());
        let before = snapshot();
        let refused = |e: anyhow::Error| {
            let text = format!("{e:#}");
            assert!(text.contains("side-conversations.v1"), "{text}");
            let status = e.downcast_ref::<Status>().expect("a gRPC status");
            assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{text}");
        };
        refused(
            owner
                .ask_side(proto::AskSideRequest {
                    agent: "Mateo".into(),
                    question: "q2".into(),
                    follows: Some(first),
                })
                .await
                .unwrap_err(),
        );
        refused(
            owner
                .cancel_side(proto::CancelSideRequest {
                    id: first,
                    end: true,
                })
                .await
                .unwrap_err(),
        );
        refused(
            runner
                .report_side(proto::ReportSideRequest {
                    generation: "run-1".into(),
                    id: first,
                    status: proto::SideStatus::SideEnded.into(),
                    answer: None,
                })
                .await
                .unwrap_err(),
        );
        assert_eq!(snapshot(), before);
        // The one-shot path is unchanged.
        let fresh = owner
            .ask_side(proto::AskSideRequest {
                agent: "Mateo".into(),
                question: "fresh".into(),
                follows: None,
            })
            .await
            .unwrap()
            .id;
        let claimed = runner
            .claim_side(proto::ClaimSideRequest {
                generation: "run-1".into(),
            })
            .await
            .unwrap()
            .request
            .unwrap();
        assert_eq!(claimed.id, fresh);
        runner
            .report_side(proto::ReportSideRequest {
                generation: "run-1".into(),
                id: fresh,
                status: proto::SideStatus::SideCompleted.into(),
                answer: Some("a2".into()),
            })
            .await
            .unwrap();
        let err = owner
            .cancel_side(proto::CancelSideRequest {
                id: fresh,
                end: false,
            })
            .await
            .unwrap_err();
        // Reached the one-shot rules: a plain cancel of a closed question.
        assert!(format!("{err:#}").contains("not open"), "{err:#}");
        let info = owner.server_info().await.unwrap();
        assert!(!info.capabilities.iter().any(|c| c == SIDE_CONVERSATIONS));
        assert!(info.capabilities.iter().any(|c| c == "side-questions.v1"));
        server.abort();
    }

    /// Agent notes over a real loopback server: an older server without
    /// agent-notes.v1 refuses both calls before any write, while a current
    /// one returns exactly what the local service stores.
    #[tokio::test]
    async fn agent_notes_refuse_old_servers_and_match_local_results() {
        use crate::backend::{AGENT_NOTES, Backend, Remote};
        for withheld in [true, false] {
            let mut conn = Connection::open_in_memory().unwrap();
            db::migrate(&mut conn).unwrap();
            let will = Caller {
                human: Some("will".into()),
                ..Default::default()
            };
            service::ensure_human(
                &mut conn,
                proto::EnsureHumanRequest {
                    name: "will".into(),
                },
            )
            .unwrap();
            let token = service::register(
                &mut conn,
                &will,
                proto::RegisterRequest {
                    name: Some("Mateo".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token;
            let mateo = service::whoami(
                &mut conn,
                &Caller {
                    token: Some(token),
                    ..Default::default()
                },
            )
            .unwrap()
            .id;
            let state = Arc::new(State {
                conn: Mutex::new(conn),
                notify: watch::Sender::new(0),
                activity: watch::Sender::new(0),
                waiters: Mutex::new(HashSet::new()),
                shutdown: watch::Sender::new(false),
                require_human_token: false,
                protocol: if withheld {
                    Policy::default().without(AGENT_NOTES)
                } else {
                    Policy::default()
                },
                auth_failures: Mutex::new(HashMap::new()),
                timer_limits: TimerLimits::default(),
                clock: Arc::new(Utc::now),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(BbsServer::new(Server(state.clone())))
                    .serve_with_incoming(TcpListenerStream::new(listener)),
            );
            let rows = || -> i64 {
                state
                    .conn
                    .lock()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM agent_notes", [], |r| r.get(0))
                    .unwrap()
            };
            let mut remote =
                Backend::Remote(Remote::connect(&address, will.clone()).await.unwrap());
            let put = remote
                .put_agent_note(proto::PutAgentNoteRequest {
                    agent_id: mateo,
                    body: "private".into(),
                })
                .await;
            if withheld {
                let error = put.unwrap_err();
                assert!(format!("{error:#}").contains(AGENT_NOTES), "{error:#}");
                let status = error.downcast_ref::<Status>().expect("a gRPC status");
                assert_eq!(status.code(), tonic::Code::FailedPrecondition);
                assert!(
                    remote
                        .list_agent_notes(proto::ListAgentNotesRequest::default())
                        .await
                        .is_err()
                );
                assert_eq!(rows(), 0, "no write on an older server");
            } else {
                let written = put.unwrap().note.unwrap();
                let listed = remote
                    .list_agent_notes(proto::ListAgentNotesRequest::default())
                    .await
                    .unwrap();
                let local = service::list_agent_notes(
                    &mut state.conn.lock().unwrap(),
                    &will,
                    proto::ListAgentNotesRequest::default(),
                )
                .unwrap();
                assert_eq!(listed, local, "remote and local agree");
                assert_eq!(listed.notes, vec![written]);
                assert_eq!(rows(), 1);
            }
            server.abort();
        }
    }

    /// Sync pages until one is caught up, each arriving without a change.
    async fn sync_pages(
        backend: &mut crate::backend::Backend,
        req: proto::FeedRequest,
    ) -> Vec<proto::FeedSnapshot> {
        let mut watch = backend.watch_feed(req, false).await.unwrap();
        let mut pages = Vec::new();
        loop {
            let page =
                tokio::time::timeout(std::time::Duration::from_secs(5), watch.next_snapshot())
                    .await
                    .expect("pages arrive at once")
                    .unwrap();
            let done = page.caught_up == Some(true);
            pages.push(page);
            if done {
                return pages;
            }
        }
    }

    #[tokio::test]
    async fn feed_sync_refuses_old_servers_and_pages_alike_remotely_and_locally() {
        use crate::backend::{Backend, FEED_SYNC, Local, Remote};
        for withheld in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut conn = db::open(&dir.path().join("bbs.db")).unwrap();
            db::migrate(&mut conn).unwrap();
            let will = Caller {
                human: Some("will".into()),
                ..Default::default()
            };
            service::ensure_human(
                &mut conn,
                proto::EnsureHumanRequest {
                    name: "will".into(),
                },
            )
            .unwrap();
            let mateo = Caller {
                token: Some(
                    service::register(
                        &mut conn,
                        &will,
                        proto::RegisterRequest {
                            name: Some("Mateo".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap()
                    .token,
                ),
                ..Default::default()
            };
            for n in 0..5 {
                service::send(
                    &mut conn,
                    &mateo,
                    proto::SendRequest {
                        to: "will".into(),
                        body: format!("mail {n}"),
                        control: None,
                    },
                )
                .unwrap();
            }
            let state = Arc::new(State {
                conn: Mutex::new(conn),
                notify: watch::Sender::new(0),
                activity: watch::Sender::new(0),
                waiters: Mutex::new(HashSet::new()),
                shutdown: watch::Sender::new(false),
                require_human_token: false,
                protocol: if withheld {
                    Policy::default().without(FEED_SYNC)
                } else {
                    Policy::default()
                },
                auth_failures: Mutex::new(HashMap::new()),
                timer_limits: TimerLimits::default(),
                clock: Arc::new(Utc::now),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(BbsServer::new(Server(state.clone())))
                    .serve_with_incoming(TcpListenerStream::new(listener)),
            );
            let req = proto::FeedRequest {
                backlog: 2,
                mine: true,
                after_id: Some(0),
                sync: true,
                ..Default::default()
            };
            let mut remote =
                Backend::Remote(Remote::connect(&address, will.clone()).await.unwrap());
            if withheld {
                let error = match remote.watch_feed(req.clone(), false).await {
                    Ok(mut watch) => watch.next_snapshot().await.unwrap_err(),
                    Err(error) => error,
                };
                assert!(format!("{error:#}").contains(FEED_SYNC), "{error:#}");
                let status = error.downcast_ref::<Status>().expect("a gRPC status");
                assert_eq!(status.code(), tonic::Code::FailedPrecondition);
                server.abort();
                continue;
            }
            let remotely = sync_pages(&mut remote, req.clone()).await;
            let mut local = Backend::Local(Local::open(dir.path(), will.clone()).unwrap());
            let locally = sync_pages(&mut local, req.clone()).await;
            assert_eq!(remotely, locally, "remote and local agree");
            let sizes: Vec<_> = remotely
                .iter()
                .map(|p| (p.messages.len(), p.caught_up))
                .collect();
            assert_eq!(sizes, [(2, Some(false)), (2, Some(false)), (1, Some(true))]);
            assert!(
                remotely.iter().flat_map(|p| &p.agent_scopes).all(|s| s
                    .agents
                    .iter()
                    .map(|a| a.name.as_str())
                    .eq(["Mateo"]))
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn terminal_error_bypasses_full_message_buffer() {
        use tokio_stream::StreamExt;
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(42)).await.unwrap();
        let (terminal, terminal_rx) = oneshot::channel();
        let mut stream = CancellableStream {
            messages: ReceiverStream::new(rx),
            terminal: Some(terminal_rx),
            done: false,
            protocol: Policy::default(),
        };
        terminal.send(Status::unauthenticated("revoked")).unwrap();
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert!(stream.next().await.is_none());
        assert!(tx.is_closed());
    }

    #[tokio::test]
    async fn an_oversized_sync_row_is_resource_exhausted_not_a_precondition() {
        let state = Arc::new(State {
            conn: Mutex::new(Connection::open_in_memory().unwrap()),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let status = state
            .call(|_| -> Result<()> { Err(service::SyncPageTooLarge { message_id: 7 }.into()) })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(
            status.message().contains("message 7"),
            "{}",
            status.message()
        );
    }

    #[tokio::test]
    async fn shutdown_finishes_active_writes_and_rejects_new_calls() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE writes (value TEXT)")
            .unwrap();
        let state = Arc::new(State {
            conn: Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let (started, active) = tokio::sync::oneshot::channel();
        let (finish, release) = std::sync::mpsc::channel();
        let writer = state.clone();
        let write = tokio::spawn(async move {
            writer
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute("INSERT INTO writes VALUES ('committed')", [])?;
                    let _ = started.send(());
                    release.recv_timeout(Duration::from_secs(5))?;
                    tx.commit()?;
                    Ok(())
                })
                .await
        });
        active.await.unwrap();
        state.shutdown.send_replace(true);
        finish.send(()).unwrap();
        write.await.unwrap().unwrap();
        let rejected = state.call(|_| Ok(())).await.unwrap_err();
        assert_eq!(rejected.code(), tonic::Code::Unavailable);
        let value: String = state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "committed");
    }

    /// A server on a free local port over a fresh board with the human will
    /// and their agent Mateo: its address and Mateo's token.
    async fn activity_board() -> (String, String) {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let will = Caller {
            human: Some("will".into()),
            ..Default::default()
        };
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let token = service::register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let state = Arc::new(State {
            conn: Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(BbsServer::new(Server(state)))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        (format!("http://{addr}"), token)
    }

    fn with_metadata<T>(message: T, key: &'static str, value: &str) -> Request<T> {
        let mut req = Request::new(message);
        req.metadata_mut().insert(key, value.parse().unwrap());
        req.metadata_mut()
            .insert("bbs-protocol-epoch", "2".parse().unwrap());
        req
    }

    fn event(record: &str) -> proto::ActivityEvent {
        proto::ActivityEvent {
            record: record.into(),
        }
    }

    #[tokio::test]
    async fn agents_post_activity_and_humans_watch_it() {
        let (addr, token) = activity_board().await;
        let mut client = proto::bbs_client::BbsClient::connect(addr).await.unwrap();
        // Humans watch; a cursor stream sends the first (empty) batch at once.
        let watch = proto::ActivityRequest {
            backlog: 2,
            after_id: Some(0),
            agent: None,
        };
        let mut stream = client
            .watch_activity(with_metadata(watch, "bbs-human", "will"))
            .await
            .unwrap()
            .into_inner();
        assert!(stream.message().await.unwrap().unwrap().events.is_empty());

        let records = [
            r#"{"ts":"t","event":"turn_start"}"#,
            r#"{"ts":"t","event":"text","text":"hi"}"#,
            r#"{"ts":"t","event":"turn_end"}"#,
        ];
        let posted = client
            .post_activity(with_metadata(
                tokio_stream::iter(records.map(event)),
                "bbs-token",
                &token,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(posted.accepted, 3);
        // A full batch of two, then the third straight after.
        let mut seen = Vec::new();
        while seen.len() < 3 {
            let batch = stream.message().await.unwrap().unwrap();
            for e in batch.events {
                assert_eq!(
                    (e.agent.as_str(), e.owner.as_deref()),
                    ("Mateo", Some("will"))
                );
                seen.push(e.record);
            }
        }
        assert_eq!(seen, records);

        // Without a cursor, the newest window, oldest first.
        let window = client
            .watch_activity(with_metadata(
                proto::ActivityRequest {
                    backlog: 2,
                    after_id: None,
                    // One agent's, named case-insensitively.
                    agent: Some("mateo".into()),
                },
                "bbs-human",
                "will",
            ))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();
        let records: Vec<_> = window.events.iter().map(|e| e.record.as_str()).collect();
        assert_eq!(records, &seen[1..]);
    }

    #[tokio::test]
    async fn bad_records_and_wrong_callers_are_refused() {
        let (addr, token) = activity_board().await;
        let mut client = proto::bbs_client::BbsClient::connect(addr).await.unwrap();
        let big = format!(
            r#"{{"event":"text","text":"{}"}}"#,
            "x".repeat(service::MAX_ACTIVITY_BYTES)
        );
        for (record, why) in [
            ("not json", "not JSON"),
            (r#"{"text":"no event"}"#, "no string \"event\""),
            (big.as_str(), "the limit is 8192"),
        ] {
            let status = client
                .post_activity(with_metadata(
                    tokio_stream::iter([event(record)]),
                    "bbs-token",
                    &token,
                ))
                .await
                .unwrap_err();
            assert!(status.message().contains(why), "{why}: {status:?}");
        }
        // Only agents post, and only humans watch.
        let status = client
            .post_activity(with_metadata(
                tokio_stream::iter([event(r#"{"event":"text"}"#)]),
                "bbs-human",
                "will",
            ))
            .await
            .unwrap_err();
        assert!(status.message().contains("requires --token"), "{status:?}");
        let status = client
            .watch_activity(with_metadata(
                proto::ActivityRequest {
                    backlog: 10,
                    after_id: None,
                    agent: None,
                },
                "bbs-token",
                &token,
            ))
            .await
            .unwrap_err();
        assert!(status.message().contains("humans only"), "{status:?}");
        let status = client
            .watch_activity(with_metadata(
                proto::ActivityRequest {
                    backlog: 10,
                    after_id: None,
                    agent: Some("Nobody".into()),
                },
                "bbs-human",
                "will",
            ))
            .await
            .unwrap_err();
        assert!(status.message().contains("no agent named"), "{status:?}");
    }

    /// A board where will's timer for Mateo was due an hour before `start`
    /// (as if the server was down), with another due 10 minutes after it.
    fn timer_board(start: DateTime<Utc>) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let will = Caller {
            human: Some("will".into()),
            ..Default::default()
        };
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        service::register(
            &mut conn,
            &will,
            proto::RegisterRequest {
                name: Some("Mateo".into()),
                ..Default::default()
            },
        )
        .unwrap();
        for (when, body) in [("in 1d", "missed"), ("in 1d", "later")] {
            let req = proto::TimerAddRequest {
                agent: "Mateo".into(),
                when: when.into(),
                body: body.into(),
            };
            service::timer_add(&mut conn, &will, req, TimerLimits::default()).unwrap();
        }
        let at = |minutes: i64| service::fmt_time(start + chrono::Duration::minutes(minutes));
        conn.execute(
            "UPDATE timers SET next_run = ?1 WHERE body = 'missed'",
            [at(-60)],
        )
        .unwrap();
        conn.execute(
            "UPDATE timers SET next_run = ?1 WHERE body = 'later'",
            [at(10)],
        )
        .unwrap();
        conn
    }

    /// The bodies of the messages timers have sent, oldest first.
    async fn fired(state: &Arc<State>) -> Vec<String> {
        state
            .call(|conn| {
                let mut stmt = conn
                    .prepare("SELECT body FROM messages WHERE timer_id IS NOT NULL ORDER BY id")?;
                let bodies = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(bodies)
            })
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn coalesced_changes_wake_on_either_signal_at_most_once_per_interval() {
        let (notify, activity) = (watch::Sender::new(0u64), watch::Sender::new(0u64));
        let mut changes =
            Changes::coalesced(notify.subscribe(), activity.subscribe(), SUMMARY_INTERVAL);
        let start = tokio::time::Instant::now();
        changes.taken();
        // A burst on both signals is one wake, a full interval after the fetch.
        for _ in 0..5 {
            notify.send_modify(|v| *v += 1);
            activity.send_modify(|v| *v += 1);
        }
        changes.changed().await;
        assert_eq!(start.elapsed(), SUMMARY_INTERVAL);
        changes.taken();
        // The burst is covered: nothing wakes until the next change, on
        // either signal; one long after the last fetch wakes at once.
        let quiet = tokio::time::timeout(Duration::from_secs(60), changes.changed()).await;
        assert!(quiet.is_err());
        activity.send_modify(|v| *v += 1);
        let before = tokio::time::Instant::now();
        changes.changed().await;
        assert_eq!(before.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn the_scheduler_fires_by_its_clock_and_catches_up_once_on_start() {
        let start = Utc::now();
        let conn = timer_board(start);
        let now = Arc::new(Mutex::new(start));
        let clock = now.clone();
        let state = Arc::new(State {
            conn: Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(move || *clock.lock().unwrap()),
        });
        let mut changes = state.notify.subscribe();
        // Wait for the scheduler to commit what `done` checks for. It bumps
        // `notify` after firing, so a check that misses it still wakes.
        async fn until(changes: &mut watch::Receiver<u64>, mut done: impl AsyncFnMut() -> bool) {
            let wait = async {
                while !done().await {
                    changes.changed().await.unwrap();
                }
            };
            tokio::time::timeout(Duration::from_secs(10), wait)
                .await
                .expect("the scheduler to fire");
        }
        tokio::spawn(run_timers(state.clone()));
        // The timer missed while "down" fires once at once; the other waits.
        until(&mut changes, async || fired(&state).await == ["missed"]).await;

        // Its time comes when the clock says so, after any change wakes the
        // scheduler (as a new timer would) rather than a real ten minutes.
        *now.lock().unwrap() = start + chrono::Duration::minutes(10);
        state.changed();
        until(&mut changes, async || {
            fired(&state).await == ["missed", "later"]
        })
        .await;
        let left = state.call(|conn| db::next_timer(conn)).await.unwrap();
        assert_eq!(left, None);
        state.shutdown.send_replace(true);
    }
}
