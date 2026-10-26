//! `bbs serve --web`: a browser UI for humans (the `web` feature). The page is
//! a React app built from `web/` and embedded at build time; its JSON API calls the gRPC service
//! in-process, so validation, authorization, the failure budget and streams
//! are exactly the gRPC server's. Signing in checks a human name and token with
//! `Whoami`; the cookie then carries both, and every call checks them again.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use data_encoding::BASE64URL_NOPAD;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_stream::{Stream, StreamExt};
use tonic::Request;
use tonic::transport::server::TcpConnectInfo;

use super::Server;
use crate::proto::bbs_server::Bbs;
use crate::proto::{self, Kind};
use crate::{protocol, release};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

const COOKIE: &str = "bbs_web";
/// Thirty days; the credential is checked on every call regardless.
const COOKIE_MAX_AGE: u32 = 30 * 24 * 60 * 60;
/// Mutating calls must carry this header, which a cross-site form cannot set
/// and a cross-origin script cannot send without a preflight we never grant.
const CSRF_HEADER: &str = "x-bbs-web";

pub async fn serve(
    listener: tokio::net::TcpListener,
    server: Server,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let app = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/info", get(info))
        .route("/api/rpc/{method}", post(rpc))
        .route("/api/watch/{kind}", get(watch))
        .fallback(static_file)
        .with_state(Arc::new(server));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .context("serving the web UI")
}

/// The signed-in human, as the cookie carries it.
struct Session {
    name: String,
    token: String,
}

impl Session {
    fn encode(&self) -> String {
        BASE64URL_NOPAD.encode(format!("{}\n{}", self.name, self.token).as_bytes())
    }

    fn from_headers(headers: &HeaderMap) -> Option<Session> {
        let cookies = headers.get_all(header::COOKIE);
        let value = cookies
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find_map(|(k, v)| (k == COOKIE).then_some(v))?;
        let decoded = BASE64URL_NOPAD.decode(value.as_bytes()).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (name, token) = text.split_once('\n')?;
        Some(Session {
            name: name.to_string(),
            token: token.to_string(),
        })
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

fn session(headers: &HeaderMap) -> Result<Session, ApiError> {
    Session::from_headers(headers)
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "not signed in"))
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

fn cookie(value: &str, max_age: u32, headers: &HeaderMap) -> HeaderValue {
    // Behind a TLS-terminating proxy the cookie should never travel in clear.
    let secure = headers
        .get("x-forwarded-proto")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"));
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
        .parse()
        .expect("cookie is ascii")
}

#[derive(serde::Deserialize)]
struct Login {
    name: String,
    token: String,
}

async fn login(
    State(server): State<Arc<Server>>,
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
    let me = server
        .whoami(request(proto::Empty {}, Some(&session), peer)?)
        .await?
        .into_inner();
    if me.kind() != Kind::Human {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "humans only"));
    }
    let session = Session {
        name: me.name.clone(),
        ..session
    };
    let mut response = Json(me).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie(&session.encode(), COOKIE_MAX_AGE, &headers),
    );
    Ok(response)
}

async fn logout(headers: HeaderMap) -> Result<Response, ApiError> {
    check_csrf(&headers)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie("", 0, &headers));
    Ok(response)
}

/// Public server facts for the sign-in page.
async fn info(
    State(server): State<Arc<Server>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    let info = server
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
    State(server): State<Arc<Server>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    check_csrf(&headers)?;
    let s = session(&headers)?;
    let s = &s;
    let b = &body[..];
    macro_rules! dispatch {
        ($($name:literal => $method:ident),* $(,)?) => {
            match method.as_str() {
                $($name => call(b, s, peer, |r| server.$method(r)).await,)*
                _ => Err(ApiError::new(StatusCode::NOT_FOUND, format!("unknown rpc {method:?}"))),
            }
        };
    }
    dispatch! {
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
    }
}

#[derive(serde::Deserialize)]
struct WatchQuery {
    /// The request message as JSON.
    #[serde(default)]
    q: Option<String>,
}

type Events = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// A watch stream as server-sent events: each snapshot is a `message` event
/// with the snapshot as JSON. A failure is one `fail` event, `{"error",
/// "status"}`, after which the stream ends; the page decides whether to retry.
async fn watch(
    State(server): State<Arc<Server>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(kind): Path<String>,
    Query(query): Query<WatchQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let s = session(&headers)?;
    let q = query.q.unwrap_or_default();
    let q = q.as_bytes();
    let events: Events = match kind.as_str() {
        "feed" => {
            let req = request(parse::<proto::FeedRequest>(q)?, Some(&s), peer)?;
            events(server.watch_feed(req).await?.into_inner())
        }
        "summary" => {
            let req = request(parse::<proto::AgentSummaryRequest>(q)?, Some(&s), peer)?;
            events(server.watch_agent_summary(req).await?.into_inner())
        }
        "activity" => activity_tail(server, parse(q)?, s, peer).await?,
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("unknown stream {kind:?}"),
            ));
        }
    };
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn events<T, S>(stream: S) -> Events
where
    T: Serialize + Send + 'static,
    S: Stream<Item = Result<T, tonic::Status>> + Send + 'static,
{
    let mut failed = false;
    Box::pin(stream.map_while(move |item| {
        if failed {
            return None;
        }
        let (status, message) = match item.map(|s| Event::default().json_data(s)) {
            Ok(Ok(event)) => return Some(Ok(event)),
            Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            Err(status) => {
                let err = ApiError::from(status);
                (err.status, err.message)
            }
        };
        failed = true;
        let data = serde_json::json!({ "error": message, "status": status.as_u16() });
        Some(Ok(Event::default().event("fail").data(data.to_string())))
    }))
}

/// An agent's activity: the newest `backlog` records first, then only newer
/// ones as they arrive, so a busy agent never resends its whole window.
async fn activity_tail(
    server: Arc<Server>,
    mut req: proto::ActivityRequest,
    session: Session,
    peer: SocketAddr,
) -> Result<Events, ApiError> {
    req.after_id = None;
    let mut window = server
        .watch_activity(request(req.clone(), Some(&session), peer)?)
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
        .watch_activity(request(req, Some(&session), peer)?)
        .await?
        .into_inner()
        // Cursor mode always sends its first batch, even when empty.
        .filter(|s| !matches!(s, Ok(s) if s.events.is_empty()));
    let stream = tokio_stream::once(Ok(first)).chain(rest);
    Ok(events(stream))
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
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
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
