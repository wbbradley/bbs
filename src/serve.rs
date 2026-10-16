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

pub async fn run(
    listen: SocketAddr,
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
    tracing::info!(%addr, state_dir = %dir.display(), "serving");
    tokio::select! {
        result = &mut server => result?,
        result = signal => {
            result?;
            health.set_not_serving::<BbsServer<Server>>().await;
            health.set_service_status("", tonic_health::ServingStatus::NotServing).await;
            state.shutdown.send_replace(true);
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
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
            .call(move |conn| service::fire_timers(conn, now))
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
        mut changes: watch::Receiver<u64>,
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
                        let skip = sent && empty && req.has_cursor();
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
                        changes.borrow_and_update();
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
        self.watch("watch_feed", req, changes).await
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

    type WatchActivityStream = BoxStream<proto::ActivitySnapshot>;

    async fn watch_activity(
        &self,
        req: Request<proto::ActivityRequest>,
    ) -> Result<Response<Self::WatchActivityStream>, Status> {
        let changes = self.0.activity.subscribe();
        self.watch("watch_activity", req, changes).await
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
