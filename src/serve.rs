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
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::db;
use crate::proto::bbs_server::{Bbs, BbsServer};
use crate::proto::{self, Kind};
use crate::protocol::Policy;
use crate::service::{self, Caller};

/// Keepalive pings let a vanished client free its waiter slot within ~30s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// A waiting agent is alive; keep it clear of stale-agent retirement.
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Leave time for process teardown within a ten-second container grace period.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);

pub async fn run(
    listen: SocketAddr,
    dir: &Path,
    require_human_token: bool,
    protocol: Policy,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(&dir.join("bbs.db"))?;
    db::migrate(&mut conn)?;
    serve(conn, listen, dir, require_human_token, protocol).await
}

async fn serve(
    conn: Connection,
    listen: SocketAddr,
    dir: &Path,
    require_human_token: bool,
    protocol: Policy,
) -> Result<()> {
    let signal = shutdown_signal()?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = listener.local_addr()?;
    let state = Arc::new(State {
        conn: Mutex::new(conn),
        notify: watch::Sender::new(0),
        waiters: Mutex::new(HashSet::new()),
        shutdown: watch::Sender::new(false),
        require_human_token,
        protocol,
        auth_failures: Mutex::new(HashMap::new()),
    });
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
    /// Inboxes with a `WaitForMail` stream open, as (kind, id).
    waiters: Mutex<HashSet<(i32, i64)>>,
    shutdown: watch::Sender<bool>,
    require_human_token: bool,
    protocol: Policy,
    auth_failures: Mutex<HashMap<IpAddr, (Instant, u32)>>,
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
        self.0.protocol.validate(req.metadata())?;
        let result = async {
            let peer = req.remote_addr().map(|addr| addr.ip());
            let caller = Arc::new(caller(&req, self.0.require_human_token));
            self.0.enforce_auth_rate(peer, &caller).await?;
            let req = req.into_inner();
            let state = self.0.clone();
            let mut changes = state.notify.subscribe();
            // The first snapshot also checks the caller is a human, before the stream opens.
            let c = caller.clone();
            let r = req.clone();
            let first = match state.call(move |conn| service::feed(conn, &c, r)).await {
                Ok(first) => first,
                Err(status) => {
                    state.record_auth_failure(peer, &status);
                    state.warn_failed("watch_feed", &caller, &status).await;
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
                            state.warn_failed("watch_feed", &caller, status).await;
                        }
                        let failed = snapshot.is_err();
                        // Cursor mode: move past this batch; a full one means
                        // more may be waiting, so fetch again without waiting.
                        let (drain, empty) = match &snapshot {
                            Ok(s) => (
                                service::advance_feed_cursor(&mut req, s),
                                s.messages.is_empty(),
                            ),
                            Err(_) => (false, false),
                        };
                        let skip = sent && empty && req.after_id.is_some();
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
                        snapshot = state.call(move |conn| service::feed(conn, &c, r)).await;
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
            Ok(Response::<Self::WatchFeedStream>::new(Box::pin(
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
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::from([(peer.ip(), (Instant::now(), 20))])),
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
            waiters: Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: Mutex::new(HashMap::new()),
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
}
