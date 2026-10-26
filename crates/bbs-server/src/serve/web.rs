//! `bbs serve --web`: a browser UI for humans. The page is
//! a React app built from `web/` and embedded at build time; its JSON API calls the gRPC service
//! in-process, so validation, authorization, the failure budget and streams
//! are exactly the gRPC server's. Signing in checks a human name and token with
//! `Whoami` and opens a session held here; the cookie carries only the
//! session's random id, and every call checks the human's token again.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use data_encoding::BASE64URL_NOPAD;
use rand::RngCore;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt, StreamMap};
use tonic::Request;
use tonic::transport::server::TcpConnectInfo;

use super::Server;
use crate::proto::bbs_server::Bbs;
use crate::proto::{self, Kind};
use crate::{protocol, release};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

/// A session ends this long after sign-in, however busy.
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// A session unused for this long ends.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 60 * 60);
/// Sessions held at once; past this the least recently used one ends.
const MAX_SESSIONS: usize = 4096;
/// Tabs one session may hold an event stream open for at once.
const STREAMS_PER_SESSION: usize = 8;
/// Watches one event stream may carry.
const MAX_SUBSCRIPTIONS: usize = 8;
const MAX_KEY_LEN: usize = 64;
/// A tab's id: what lets a tab's next stream replace its last at once.
const MAX_TAB_LEN: usize = 64;
/// How often an open event stream checks its session is still signed in.
const SESSION_CHECK: Duration = Duration::from_secs(5);
/// Mutating calls must carry this header, which a cross-site form cannot set
/// and a cross-origin script cannot send without a preflight we never grant.
const CSRF_HEADER: &str = "x-bbs-web";

pub async fn serve(
    listener: tokio::net::TcpListener,
    server: Server,
    boundary: Boundary,
) -> Result<()> {
    let state = server.0.clone();
    let app = Arc::new(App {
        server,
        sessions: Arc::default(),
        boundary,
    });
    let router = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/info", get(info))
        .route("/api/rpc/{method}", post(rpc))
        .route("/api/events", get(events))
        .route("/healthz", get(healthz))
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { state.stopping().await })
    .await
    .context("serving the web UI")
}

struct App {
    server: Server,
    sessions: Arc<Sessions>,
    boundary: Boundary,
}

/// Where browsers reach the web UI: the hosts and origins its requests may
/// carry, and whether the session cookie travels only over TLS.
pub struct Boundary {
    origin: String,
    hosts: Vec<String>,
    origins: Vec<String>,
    secure: bool,
}

impl Boundary {
    /// For the web UI bound at `bound`. Without `origin`, a loopback address
    /// is its own origin; any other address needs one, so that serving
    /// sessions in plain text across a network is a stated choice.
    pub fn new(bound: SocketAddr, origin: Option<&str>) -> Result<Boundary> {
        let loopback = bound.ip().is_loopback();
        let port = bound.port();
        let local = if !loopback {
            Vec::new()
        } else if bound.ip().is_ipv6() {
            vec![format!("[::1]:{port}"), format!("localhost:{port}")]
        } else {
            vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")]
        };
        let (origin, secure, host) = match origin {
            Some(origin) => parse_origin(origin)?,
            None if loopback => (format!("http://{}", local[0]), false, local[0].clone()),
            None => bail!(
                "--web {bound} is not a loopback address; pass --web-origin with the URL browsers use, https:// behind a TLS proxy"
            ),
        };
        if !secure && !loopback {
            tracing::warn!(
                %origin,
                "the web UI serves sign-in and sessions in plain text; use an https origin behind a TLS proxy unless the network itself is private and encrypted"
            );
        }
        let mut hosts = vec![host];
        let mut origins = vec![origin.clone()];
        for host in local {
            if !hosts.contains(&host) {
                origins.push(format!("http://{host}"));
                hosts.push(host);
            }
        }
        Ok(Boundary {
            origin,
            hosts,
            origins,
            secure,
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let default = if self.secure { ":443" } else { ":80" };
        self.hosts
            .iter()
            .any(|h| *h == host || format!("{h}{default}") == host)
    }

    fn allows_origin(&self, origin: &str) -> bool {
        let origin = origin.to_ascii_lowercase();
        self.origins.contains(&origin)
    }

    /// `__Host-` makes browsers refuse the cookie from any other host or
    /// path, so a sibling site cannot plant one; it needs `Secure`.
    fn cookie_name(&self) -> &'static str {
        if self.secure {
            "__Host-bbs_session"
        } else {
            "bbs_session"
        }
    }

    fn cookie(&self, value: &str, max_age: Duration) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
            self.cookie_name(),
            max_age.as_secs()
        )
        .parse()
        .expect("cookie is ascii")
    }

    fn clear_cookie(&self) -> HeaderValue {
        self.cookie("", Duration::ZERO)
    }
}

/// `scheme://authority` from `--web-origin`, lowercased, without a default
/// port: (origin, whether https, authority).
fn parse_origin(origin: &str) -> Result<(String, bool, String)> {
    let trimmed = origin.trim().trim_end_matches('/');
    let (scheme, authority) = trimmed
        .split_once("://")
        .with_context(|| format!("--web-origin {origin:?} needs a scheme, such as https://"))?;
    let scheme = scheme.to_ascii_lowercase();
    let secure = match scheme.as_str() {
        "https" => true,
        "http" => false,
        _ => bail!("--web-origin {origin:?} must be http:// or https://"),
    };
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@'])
        || authority.contains(char::is_whitespace)
    {
        bail!("--web-origin {origin:?} must be scheme://host[:port], with no path");
    }
    let mut authority = authority.to_ascii_lowercase();
    let default = if secure { ":443" } else { ":80" };
    if let Some(host) = authority.strip_suffix(default) {
        authority = host.to_string();
    }
    Ok((format!("{scheme}://{authority}"), secure, authority))
}

/// Every request must name one of our hosts, which keeps a rebound DNS name
/// from reaching the API; API calls must come from our own
/// pages; and API responses are never stored by browsers or proxies.
async fn guard(State(app): State<Arc<App>>, req: axum::extract::Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !host.is_some_and(|h| app.boundary.allows_host(h)) {
        return ApiError::new(StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    let api = req.uri().path().starts_with("/api/");
    if api {
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
        let foreign = match origin {
            Some(origin) => !app.boundary.allows_origin(origin),
            // Browsers send Origin on every POST.
            None => req.method() != Method::GET && req.method() != Method::HEAD,
        } || site.is_some_and(|s| !matches!(s, "same-origin" | "none"));
        if foreign {
            return ApiError::new(StatusCode::FORBIDDEN, "cross-origin request").into_response();
        }
    }
    let mut response = next.run(req).await;
    if api {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Readiness: the web listener answers, at the web UI's own host.
async fn healthz() -> &'static str {
    "ok\n"
}

/// The signed-in human's credentials, held here only.
#[derive(Clone)]
struct Session {
    name: String,
    token: String,
}

struct Entry {
    session: Session,
    created: Instant,
    used: Instant,
    /// Each tab's open event stream: dropping its lease ends that stream.
    tabs: HashMap<String, Lease>,
}

/// A tab's hold on its event stream. Dropping it, by the tab opening
/// another or the session ending, ends the stream at once.
struct Lease {
    generation: u64,
    _cancel: oneshot::Sender<()>,
}

impl Entry {
    fn live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) < SESSION_LIFETIME
            && now.saturating_duration_since(self.used) < SESSION_IDLE
    }
}

/// Open sessions, by a hash of their id: logging out, expiry, or the token
/// failing ends one at once, and a restart ends them all.
#[derive(Default)]
struct Sessions {
    entries: Mutex<HashMap<[u8; 32], Entry>>,
    /// Leases issued, so a replaced one never releases its successor.
    generation: std::sync::atomic::AtomicU64,
}

fn session_key(id: &str) -> Option<[u8; 32]> {
    let raw = BASE64URL_NOPAD.decode(id.as_bytes()).ok()?;
    (raw.len() == 32).then(|| Sha256::digest(&raw).into())
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new session's id, for the cookie.
    fn open(&self, session: Session, now: Instant) -> String {
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let id = BASE64URL_NOPAD.encode(&raw);
        let mut sessions = self.lock();
        sessions.retain(|_, e| e.live(now));
        if sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = sessions.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| *k)
        {
            sessions.remove(&oldest);
        }
        sessions.insert(
            Sha256::digest(raw).into(),
            Entry {
                session,
                created: now,
                used: now,
                tabs: HashMap::new(),
            },
        );
        id
    }

    /// The session `id` names, if it is still live; using it keeps it so.
    fn get(&self, id: &str, now: Instant) -> Option<Session> {
        let key = session_key(id)?;
        let mut sessions = self.lock();
        let entry = sessions.get_mut(&key)?;
        if !entry.live(now) {
            sessions.remove(&key);
            return None;
        }
        entry.used = now;
        Some(entry.session.clone())
    }

    /// Whether `id` is still live, without counting as use: an open stream
    /// is not activity.
    fn alive(&self, id: &str, now: Instant) -> bool {
        let Some(key) = session_key(id) else {
            return false;
        };
        self.lock().get(&key).is_some_and(|e| e.live(now))
    }

    fn close(&self, id: &str) {
        if let Some(key) = session_key(id) {
            self.lock().remove(&key);
        }
    }

    /// Tab `tab`'s event stream on session `id`: any stream the tab had
    /// ends now, and the new one ends when its cancel resolves. Distinct
    /// tabs are capped; a tab replacing its own stream never is.
    fn lease(
        self: &Arc<Self>,
        id: &str,
        tab: &str,
    ) -> Result<(StreamSlot, oneshot::Receiver<()>), ApiError> {
        let not_signed_in = || ApiError::new(StatusCode::UNAUTHORIZED, "not signed in");
        let key = session_key(id).ok_or_else(not_signed_in)?;
        let mut sessions = self.lock();
        let entry = sessions.get_mut(&key).ok_or_else(not_signed_in)?;
        if !entry.tabs.contains_key(tab) && entry.tabs.len() >= STREAMS_PER_SESSION {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("at most {STREAMS_PER_SESSION} tabs may stream per session"),
            ));
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (cancel, cancelled) = oneshot::channel();
        // Replacing the tab's lease drops the old one, ending its stream.
        entry.tabs.insert(
            tab.to_string(),
            Lease {
                generation,
                _cancel: cancel,
            },
        );
        Ok((
            StreamSlot {
                sessions: self.clone(),
                key,
                tab: tab.to_string(),
                generation,
            },
            cancelled,
        ))
    }
}

/// A stream's place in its session, given up when the stream ends, unless
/// the tab has since opened another.
struct StreamSlot {
    sessions: Arc<Sessions>,
    key: [u8; 32],
    tab: String,
    generation: u64,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        if let Some(entry) = self.sessions.lock().get_mut(&self.key)
            && entry
                .tabs
                .get(&self.tab)
                .is_some_and(|l| l.generation == self.generation)
        {
            entry.tabs.remove(&self.tab);
        }
    }
}

/// A request as a current client would send it, from `peer`, so the auth
/// failure budget counts browsers as it counts gRPC clients.
fn request<T>(
    message: T,
    session: Option<&Session>,
    peer: SocketAddr,
) -> Result<Request<T>, ApiError> {
    let mut req = Request::new(message);
    let metadata = req.metadata_mut();
    metadata.insert(
        "bbs-protocol-epoch",
        protocol::EPOCH.to_string().parse().expect("ascii"),
    );
    metadata.insert(
        "bbs-client-version",
        release::VERSION.parse().expect("ascii"),
    );
    if let Some(session) = session {
        let value = |s: &str| {
            s.parse()
                .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid credentials"))
        };
        metadata.insert("bbs-human", value(&session.name)?);
        metadata.insert("bbs-human-token", value(&session.token)?);
    }
    req.extensions_mut().insert(TcpConnectInfo {
        local_addr: None,
        remote_addr: Some(peer),
    });
    Ok(req)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
        }
    }
}

impl From<tonic::Status> for ApiError {
    fn from(status: tonic::Status) -> ApiError {
        use tonic::Code;
        let code = match status.code() {
            Code::Unauthenticated => StatusCode::UNAUTHORIZED,
            Code::PermissionDenied => StatusCode::FORBIDDEN,
            Code::NotFound => StatusCode::NOT_FOUND,
            Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
            Code::Internal | Code::Unknown | Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError::new(code, status.message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

/// The cookie's session id, whether or not it is still live.
fn session_id(app: &App, headers: &HeaderMap) -> Option<String> {
    let name = app.boundary.cookie_name();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(k, v)| (k == name).then(|| v.to_string()))
}

/// The live session the cookie names, and its id.
fn signed_in(app: &App, headers: &HeaderMap) -> Result<(String, Session), ApiError> {
    session_id(app, headers)
        .and_then(|id| Some((id.clone(), app.sessions.get(&id, Instant::now())?)))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "not signed in"))
}

/// A call's outcome; a credential the server no longer accepts, or a session
/// that has ended, ends the session, and the browser forgets it.
fn finish(app: &App, id: &str, result: Result<Response, ApiError>) -> Response {
    match result {
        Ok(response) => response,
        Err(e) => {
            let unauthorized = e.status == StatusCode::UNAUTHORIZED;
            let mut response = e.into_response();
            if unauthorized {
                app.sessions.close(id);
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, app.boundary.clear_cookie());
            }
            response
        }
    }
}

fn check_csrf(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.get(CSRF_HEADER).is_none() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("missing {CSRF_HEADER} header"),
        ));
    }
    Ok(())
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    let body = if body.is_empty() {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid request: {e}")))
}

#[derive(serde::Deserialize)]
struct Login {
    name: String,
    token: String,
}

async fn login(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    check_csrf(&headers)?;
    let login: Login = parse(&body)?;
    let session = Session {
        name: login.name.trim().to_string(),
        token: login.token.trim().to_string(),
    };
    if session.name.is_empty() || session.token.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "enter your name and human token",
        ));
    }
    let me = app
        .server
        .whoami(request(proto::Empty {}, Some(&session), peer)?)
        .await?
        .into_inner();
    if me.kind() != Kind::Human {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "humans only"));
    }
    // Signing in again never keeps the session it replaces.
    if let Some(old) = session_id(&app, &headers) {
        app.sessions.close(&old);
    }
    let id = app.sessions.open(
        Session {
            name: me.name.clone(),
            ..session
        },
        Instant::now(),
    );
    let mut response = Json(me).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        app.boundary.cookie(&id, SESSION_LIFETIME),
    );
    Ok(response)
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Response, ApiError> {
    check_csrf(&headers)?;
    if let Some(id) = session_id(&app, &headers) {
        app.sessions.close(&id);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, app.boundary.clear_cookie());
    Ok(response)
}

/// Public server facts for the sign-in page.
async fn info(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    let info = app
        .server
        .server_info(request(proto::Empty {}, None, peer)?)
        .await?
        .into_inner();
    Ok(Json(info).into_response())
}

async fn call<Req, Resp, F, Fut>(
    body: &[u8],
    session: &Session,
    peer: SocketAddr,
    f: F,
) -> Result<Response, ApiError>
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: FnOnce(Request<Req>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
{
    let req = request(parse::<Req>(body)?, Some(session), peer)?;
    let resp = f(req).await?.into_inner();
    Ok(Json(resp).into_response())
}

/// The unary RPCs the page uses, by their snake_case names, with JSON bodies
/// in the proto's field names. Agent-only and enrollment RPCs are not here.
async fn rpc(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    check_csrf(&headers)?;
    let (id, s) = match signed_in(&app, &headers) {
        Ok(signed_in) => signed_in,
        Err(e) => return Ok(finish(&app, "", Err(e))),
    };
    let s = &s;
    let b = &body[..];
    let server = &app.server;
    macro_rules! dispatch {
        ($($name:literal => $method:ident),* $(,)?) => {
            match method.as_str() {
                $($name => call(b, s, peer, |r| server.$method(r)).await,)*
                _ => Err(ApiError::new(StatusCode::NOT_FOUND, format!("unknown rpc {method:?}"))),
            }
        };
    }
    let result = dispatch! {
        "whoami" => whoami,
        "list_agents" => list_agents,
        "retire" => retire,
        "group_list" => group_list,
        "group_register" => group_register,
        "group_delete" => group_delete,
        "group_describe" => group_describe,
        "group_add" => group_add,
        "group_remove" => group_remove,
        "set_group_writer" => set_group_writer,
        "send" => send,
        "search" => search,
        "mark_read" => mark_read,
        "mark_seen" => mark_seen,
        "side_report" => side_report,
        "answer_owner_input" => answer_owner_input,
        "ask_side" => ask_side,
        "cancel_side" => cancel_side,
        "list_blockers" => list_blockers,
        "answer_blocker" => answer_blocker,
        "cancel_blocker" => cancel_blocker,
        "list_agent_notes" => list_agent_notes,
        "put_agent_note" => put_agent_note,
    };
    Ok(finish(&app, &id, result))
}

/// One watch's snapshots, as JSON, ending after its first failure.
type Snapshots = Pin<Box<dyn Stream<Item = Result<serde_json::Value, ApiError>> + Send>>;

fn snapshots<T, S>(stream: S) -> Snapshots
where
    T: Serialize + Send + 'static,
    S: Stream<Item = Result<T, tonic::Status>> + Send + 'static,
{
    let mut failed = false;
    Box::pin(stream.map_while(move |item| {
        if failed {
            return None;
        }
        let item = item.map_err(ApiError::from).and_then(|s| {
            serde_json::to_value(s)
                .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        });
        failed = item.is_err();
        Some(item)
    }))
}

/// Open the watch `kind` with request `q` (JSON) for `session`.
async fn open_watch(
    app: &Arc<App>,
    kind: &str,
    q: &[u8],
    session: &Session,
    peer: SocketAddr,
) -> Result<Snapshots, ApiError> {
    let server = &app.server;
    Ok(match kind {
        "feed" => {
            let req = request(parse::<proto::FeedRequest>(q)?, Some(session), peer)?;
            snapshots(server.watch_feed(req).await?.into_inner())
        }
        "summary" => {
            let req = request(parse::<proto::AgentSummaryRequest>(q)?, Some(session), peer)?;
            snapshots(server.watch_agent_summary(req).await?.into_inner())
        }
        "activity" => activity_tail(server, parse(q)?, session, peer).await?,
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("unknown stream {kind:?}"),
            ));
        }
    })
}

/// An agent's activity: the newest `backlog` records first, then only newer
/// ones as they arrive, so a busy agent never resends its whole window. With
/// `after_id`, a page reconnecting after records it has: only newer ones,
/// in batches, from the first.
async fn activity_tail(
    server: &Server,
    mut req: proto::ActivityRequest,
    session: &Session,
    peer: SocketAddr,
) -> Result<Snapshots, ApiError> {
    if req.after_id.is_some() {
        let rest = server
            .watch_activity(request(req, Some(session), peer)?)
            .await?
            .into_inner();
        return Ok(snapshots(rest));
    }
    let mut window = server
        .watch_activity(request(req.clone(), Some(session), peer)?)
        .await?
        .into_inner();
    let first = match window.next().await {
        Some(first) => first?,
        None => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "stream ended",
            ));
        }
    };
    drop(window);
    let after = first.events.iter().map(|e| e.id).max().unwrap_or(0);
    req.after_id = Some(after);
    let rest = server
        .watch_activity(request(req, Some(session), peer)?)
        .await?
        .into_inner()
        // Cursor mode always sends its first batch, even when empty.
        .filter(|s| !matches!(s, Ok(s) if s.events.is_empty()));
    Ok(snapshots(tokio_stream::once(Ok(first)).chain(rest)))
}

/// What a relayed stream sends next: an event, or its last one.
enum Out {
    Event(Event),
    Last(Event),
}

type Events = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn fail_event(key: Option<&str>, status: StatusCode, message: &str) -> Event {
    let data = serde_json::json!({ "key": key, "error": message, "status": status.as_u16() });
    Event::default().event("fail").data(data.to_string())
}

/// Relay `source` to the browser while session `id` stays signed in, its tab
/// opens no other stream, and the server is not stopping; holds `slot`
/// until then or until the browser goes. A send blocked on a slow browser
/// gives way to any of those too.
fn relay(
    app: Arc<App>,
    id: String,
    slot: StreamSlot,
    mut cancelled: oneshot::Receiver<()>,
    mut source: Pin<Box<dyn Stream<Item = Out> + Send>>,
) -> Events {
    let (tx, rx) = mpsc::channel::<Event>(16);
    tokio::spawn(async move {
        let _slot = slot;
        let state = app.server.0.clone();
        let mut check = tokio::time::interval(SESSION_CHECK);
        check.tick().await;
        // Why the relay stops, if not for the browser or the stream ending.
        enum Stop {
            Ended,
            Cancelled,
        }
        loop {
            let next = tokio::select! {
                _ = state.stopping() => Stop::Ended,
                _ = tx.closed() => Stop::Ended,
                _ = &mut cancelled => Stop::Cancelled,
                _ = check.tick() => {
                    if app.sessions.alive(&id, Instant::now()) {
                        continue;
                    }
                    Stop::Cancelled
                }
                out = source.next() => {
                    let (event, last) = match out {
                        Some(Out::Event(event)) => (event, false),
                        Some(Out::Last(event)) => (event, true),
                        None => break,
                    };
                    tokio::select! {
                        sent = tx.send(event) => {
                            if sent.is_err() || last {
                                break;
                            }
                            continue;
                        }
                        _ = state.stopping() => Stop::Ended,
                        _ = &mut cancelled => Stop::Cancelled,
                    }
                }
            };
            // Signed out (rather than replaced by the tab's next stream):
            // say so, without waiting on a browser that is not reading.
            if let Stop::Cancelled = next
                && !app.sessions.alive(&id, Instant::now())
            {
                let _ = tx.try_send(fail_event(None, StatusCode::UNAUTHORIZED, "signed out"));
            }
            break;
        }
    });
    Box::pin(ReceiverStream::new(rx).map(Ok))
}

fn sse(events: Events) -> Response {
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    /// The watches as JSON: `[{"key", "kind", "q"}]`.
    s: String,
    /// The tab's own id; its next stream replaces this one.
    tab: String,
}

#[derive(serde::Deserialize)]
struct Subscription {
    key: String,
    kind: String,
    #[serde(default)]
    q: serde_json::Value,
}

/// Several watches over one event stream, so a tab holds one connection
/// however many it shows: each snapshot is a `message` event `{"key",
/// "snapshot"}`. A watch that fails sends `fail` `{"key", "error", "status"}`
/// and ends alone; `fail` with a null key means the session ended, and the
/// stream with it.
async fn events(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (id, session) = match signed_in(&app, &headers) {
        Ok(signed_in) => signed_in,
        Err(e) => return Ok(finish(&app, "", Err(e))),
    };
    let tab = query.tab;
    if tab.is_empty()
        || tab.len() > MAX_TAB_LEN
        || !tab
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("tab must be 1 to {MAX_TAB_LEN} letters, digits, - or _"),
        ));
    }
    let subscriptions: Vec<Subscription> = parse(query.s.as_bytes())?;
    if subscriptions.is_empty() || subscriptions.len() > MAX_SUBSCRIPTIONS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("watch between 1 and {MAX_SUBSCRIPTIONS} streams"),
        ));
    }
    // The tab's last stream ends before this one's watches open.
    let (slot, cancelled) = match app.sessions.lease(&id, &tab) {
        Ok(lease) => lease,
        Err(e) => return Ok(finish(&app, &id, Err(e))),
    };
    let mut map = StreamMap::new();
    for sub in subscriptions {
        if sub.key.is_empty() || sub.key.len() > MAX_KEY_LEN || map.contains_key(&sub.key) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("each key must be distinct and 1 to {MAX_KEY_LEN} bytes"),
            ));
        }
        let q = if sub.q.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&sub.q).expect("a JSON value")
        };
        let stream = match open_watch(&app, &sub.kind, &q, &session, peer).await {
            Ok(stream) => stream,
            Err(e) if e.status == StatusCode::UNAUTHORIZED => {
                return Ok(finish(&app, &id, Err(e)));
            }
            Err(e) => Box::pin(tokio_stream::once(Err(e))),
        };
        map.insert(sub.key, stream);
    }
    let (closing, closing_id) = (app.clone(), id.clone());
    let source = map.map(move |(key, item)| match item {
        Ok(snapshot) => Out::Event(
            Event::default()
                .json_data(serde_json::json!({ "key": key, "snapshot": snapshot }))
                .expect("JSON"),
        ),
        // The credential no longer works: the session ends with it.
        Err(e) if e.status == StatusCode::UNAUTHORIZED => {
            closing.sessions.close(&closing_id);
            Out::Last(fail_event(None, e.status, &e.message))
        }
        Err(e) => Out::Event(fail_event(Some(&key), e.status, &e.message)),
    });
    Ok(sse(relay(app, id, slot, cancelled, Box::pin(source))))
}

/// The embedded page and its assets; unknown paths get the page, which routes
/// them itself.
async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    }
    // build.rs guarantees an index.html.
    let Some((name, bytes)) = assets::ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .or_else(|| {
            assets::ASSETS
                .iter()
                .find(|(name, _)| *name == "index.html")
        })
    else {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    };
    // Vite names built assets by content hash, so they never change.
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, content_type(name))
        .header(header::CACHE_CONTROL, cache)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "same-origin")
        .header(
            "content-security-policy",
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        )
        .body(Body::from(*bytes))
        .expect("valid response")
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            name: "ada".into(),
            token: "t".into(),
        }
    }

    #[test]
    fn sessions_end_on_close_idle_and_lifetime() {
        let sessions = Sessions::default();
        let start = Instant::now();
        let id = sessions.open(session(), start);
        assert!(sessions.get(&id, start).is_some());
        sessions.close(&id);
        assert!(sessions.get(&id, start).is_none(), "closed");
        // Idle: unused for a day.
        let id = sessions.open(session(), start);
        assert!(
            sessions
                .get(&id, start + SESSION_IDLE - Duration::from_secs(1))
                .is_some()
        );
        let later = start + SESSION_IDLE - Duration::from_secs(1);
        assert!(sessions.get(&id, later + SESSION_IDLE).is_none(), "idle");
        // Lifetime: used every hour, still ends after thirty days.
        let id = sessions.open(session(), start);
        let mut at = start;
        while at < start + SESSION_LIFETIME - Duration::from_secs(3600) {
            at += Duration::from_secs(3600);
            assert!(sessions.get(&id, at).is_some());
        }
        assert!(
            sessions.get(&id, start + SESSION_LIFETIME).is_none(),
            "lifetime"
        );
        // Watching is not use.
        let id = sessions.open(session(), start);
        assert!(sessions.alive(&id, start + SESSION_IDLE - Duration::from_secs(1)));
        assert!(!sessions.alive(&id, start + SESSION_IDLE));
    }

    #[test]
    fn ids_are_random_and_never_the_token() {
        let sessions = Sessions::default();
        let now = Instant::now();
        let a = sessions.open(session(), now);
        let b = sessions.open(session(), now);
        assert_ne!(a, b);
        assert_eq!(BASE64URL_NOPAD.decode(a.as_bytes()).unwrap().len(), 32);
        assert!(sessions.get("not-an-id", now).is_none());
        assert!(
            sessions
                .get(&BASE64URL_NOPAD.encode(&[0u8; 32]), now)
                .is_none()
        );
    }

    #[test]
    fn the_least_recently_used_session_gives_way_at_the_cap() {
        let sessions = Sessions::default();
        let start = Instant::now();
        let first = sessions.open(session(), start);
        let mut last = String::new();
        for i in 1..MAX_SESSIONS {
            last = sessions.open(session(), start + Duration::from_millis(i as u64));
        }
        let now = start + Duration::from_secs(10);
        assert!(sessions.get(&last, now).is_some());
        sessions.open(session(), now);
        assert!(sessions.get(&first, now).is_none(), "evicted");
        assert!(sessions.get(&last, now).is_some());
    }

    #[test]
    fn a_replaced_lease_never_releases_its_successor() {
        let sessions = Arc::new(Sessions::default());
        let id = sessions.open(session(), Instant::now());
        let (old, mut old_cancelled) = sessions.lease(&id, "t").unwrap();
        let (new, mut new_cancelled) = sessions.lease(&id, "t").unwrap();
        assert!(
            old_cancelled
                .try_recv()
                .is_err_and(|e| e == oneshot::error::TryRecvError::Closed)
        );
        drop(old);
        assert!(
            new_cancelled
                .try_recv()
                .is_err_and(|e| e == oneshot::error::TryRecvError::Empty)
        );
        drop(new);
        assert!(
            new_cancelled
                .try_recv()
                .is_err_and(|e| e == oneshot::error::TryRecvError::Closed)
        );
        // Closing the session ends every lease it holds.
        let (_slot, mut cancelled) = sessions.lease(&id, "u").unwrap();
        sessions.close(&id);
        assert!(
            cancelled
                .try_recv()
                .is_err_and(|e| e == oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn origins_and_hosts() {
        let loopback: SocketAddr = "127.0.0.1:7778".parse().unwrap();
        let b = Boundary::new(loopback, None).unwrap();
        assert_eq!(b.origin(), "http://127.0.0.1:7778");
        assert!(b.allows_host("127.0.0.1:7778") && b.allows_host("LOCALHOST:7778"));
        assert!(!b.allows_host("evil.example:7778") && !b.allows_host("127.0.0.1:7779"));
        assert!(b.allows_origin("http://localhost:7778"));
        assert!(!b.allows_origin("http://evil.example:7778"));
        assert_eq!(b.cookie_name(), "bbs_session");
        assert!(
            !b.cookie("x", SESSION_LIFETIME)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
        // Anywhere else needs an origin.
        let any: SocketAddr = "0.0.0.0:7778".parse().unwrap();
        assert!(Boundary::new(any, None).is_err());
        let b = Boundary::new(any, Some("https://BBS.example.com:443/")).unwrap();
        assert_eq!(b.origin(), "https://bbs.example.com");
        assert!(b.allows_host("bbs.example.com") && b.allows_host("bbs.example.com:443"));
        assert!(!b.allows_host("0.0.0.0:7778") && !b.allows_host("127.0.0.1:7778"));
        assert!(b.allows_origin("https://bbs.example.com"));
        assert!(!b.allows_origin("http://bbs.example.com"));
        assert_eq!(b.cookie_name(), "__Host-bbs_session");
        let cookie = b.cookie("x", SESSION_LIFETIME);
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with("__Host-bbs_session=x;") && cookie.ends_with("; Secure"));
        let http = Boundary::new(any, Some("http://bbs:7778")).unwrap();
        assert!(http.allows_host("bbs:7778") && !http.secure);
        for bad in [
            "bbs.example.com",
            "ftp://bbs",
            "https://bbs/x",
            "https://u@bbs",
            "https://",
        ] {
            assert!(Boundary::new(any, Some(bad)).is_err(), "{bad}");
        }
    }
}
