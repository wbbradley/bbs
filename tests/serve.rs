//! Drive `bbs serve` over gRPC with the generated client.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tonic::Request;
use tonic::transport::Channel;
use tonic_health::pb::{
    HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
};

mod proto {
    tonic::include_proto!("bbs.v2");
}

use proto::bbs_client::BbsClient;

type Client = BbsClient<
    tonic::service::interceptor::InterceptedService<
        Channel,
        fn(Request<()>) -> Result<Request<()>, tonic::Status>,
    >,
>;

fn with_contract(mut req: Request<()>) -> Result<Request<()>, tonic::Status> {
    req.metadata_mut()
        .insert("bbs-protocol-epoch", "2".parse().unwrap());
    req.metadata_mut().insert(
        "bbs-client-version",
        env!("CARGO_PKG_VERSION").parse().unwrap(),
    );
    req.metadata_mut().insert(
        "bbs-required-capabilities",
        "named-agents.v1,owner-attribution.v1,human-token-sessions.v1"
            .parse()
            .unwrap(),
    );
    Ok(req)
}

struct Server {
    child: Child,
    home: PathBuf,
    addr: String,
}

impl Server {
    fn start(test: &str) -> Server {
        Self::start_mode(test, false)
    }

    fn start_mode(test: &str, protected: bool) -> Server {
        Self::start_options(test, protected, &[])
    }

    fn start_options(test: &str, protected: bool, args: &[&str]) -> Server {
        let home = std::env::temp_dir().join(format!("bbs-serve-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bbs"));
        command
            .args(["serve", "--listen", "127.0.0.1:0"])
            .args(args);
        if protected {
            command.arg("--require-human-token");
        }
        let mut child = command
            .env("HOME", &home)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .rsplit(' ')
            .next()
            .expect("listening line")
            .to_string();
        Server { child, home, addr }
    }

    async fn raw_client(&self) -> BbsClient<Channel> {
        BbsClient::connect(format!("http://{}", self.addr))
            .await
            .unwrap()
    }

    async fn client(&self) -> Client {
        let channel = Channel::from_shared(format!("http://{}", self.addr))
            .unwrap()
            .connect()
            .await
            .unwrap();
        BbsClient::with_interceptor(channel, with_contract as fn(_) -> _)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn as_agent<T>(token: &str, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-token", token.parse().unwrap());
    req
}

fn as_human<T>(name: &str, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-human", name.parse().unwrap());
    req
}

fn contract<T>(msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-protocol-epoch", "2".parse().unwrap());
    req.metadata_mut().insert(
        "bbs-client-version",
        env!("CARGO_PKG_VERSION").parse().unwrap(),
    );
    req
}

#[tokio::test]
async fn incompatible_requests_do_not_touch_state_or_claim_stream_slots() {
    let server = Server::start("protocol-effects");
    let mut client = server.client().await;
    let agent = register(&mut client).await;
    client
        .send(as_human(
            "will",
            proto::SendRequest {
                to: agent.name.clone(),
                body: "unread".into(),
            },
        ))
        .await
        .unwrap();
    let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
    db.execute(
        "UPDATE humans SET active_at = '2001-01-01T00:00:00.000000Z'",
        [],
    )
    .unwrap();
    let before: String = db
        .query_row(
            "SELECT active_at FROM humans WHERE name = 'will'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut raw = server.raw_client().await;
    let err = raw
        .ensure_human(proto::EnsureHumanRequest {
            name: "intruder".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert_eq!(err.metadata().get("bbs-protocol-epoch").unwrap(), "2");
    let mut request = contract(proto::RegisterRequest {
        name: Some("MustNotExist".into()),
        ..Default::default()
    });
    request
        .metadata_mut()
        .insert("bbs-human", "will".parse().unwrap());
    request.metadata_mut().insert(
        "bbs-required-capabilities",
        "named-agents.v1,unknown.v1".parse().unwrap(),
    );
    assert!(
        raw.register(request)
            .await
            .unwrap_err()
            .message()
            .contains("unknown.v1")
    );
    assert!(
        raw.watch_feed(as_human(
            "will",
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            }
        ))
        .await
        .is_err()
    );
    assert!(
        raw.wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
            .await
            .is_err()
    );
    assert!(
        raw.read(as_agent(&agent.token, proto::ReadRequest::default()))
            .await
            .is_err()
    );
    assert!(
        raw.bind_session(proto::BindSessionRequest {
            harness: "test".into(),
            session_id: "rejected".into(),
            token: agent.token.clone()
        })
        .await
        .is_err()
    );
    let after: String = db
        .query_row(
            "SELECT active_at FROM humans WHERE name = 'will'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    for query in [
        "SELECT COUNT(*) FROM humans WHERE name = 'intruder'",
        "SELECT COUNT(*) FROM agents WHERE name = 'MustNotExist'",
        "SELECT COUNT(*) FROM sessions WHERE session_id = 'rejected'",
    ] {
        assert_eq!(db.query_row(query, [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
    let response = client
        .wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap();
    assert_eq!(
        response.metadata().get("bbs-server-version").unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    let mut stream = response.into_inner();
    assert_eq!(stream.message().await.unwrap().unwrap().body, "unread");
    assert!(stream.message().await.unwrap().is_none());
    // The server sends hints as soon as an idle stream opens, without waiting for mail.
    let idle = client
        .wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap();
    assert_eq!(idle.metadata().get("bbs-protocol-epoch").unwrap(), "2");
}

#[tokio::test]
async fn minimum_policy_preserves_public_diagnostics_and_error_hints() {
    let server = Server::start_options(
        "protocol-minimum",
        false,
        &[
            "--minimum-client-version",
            "9999.0.0",
            "--recommended-client-version",
            "9999.1.0",
        ],
    );
    let mut raw = server.raw_client().await;
    let info = raw.server_info(proto::Empty {}).await.unwrap().into_inner();
    assert_eq!(info.protocol_epoch, Some(2));
    assert_eq!(info.release.unwrap().version, env!("CARGO_PKG_VERSION"));
    assert_eq!(info.minimum_client_version.as_deref(), Some("9999.0.0"));
    assert_eq!(info.recommended_client_version.as_deref(), Some("9999.1.0"));
    let mut client = server.client().await;
    let rejected = client
        .ensure_human(proto::EnsureHumanRequest { name: "old".into() })
        .await
        .unwrap_err();
    assert!(rejected.message().contains("upgrade the client"));
    assert_eq!(
        rejected
            .metadata()
            .get("bbs-minimum-client-version")
            .unwrap(),
        "9999.0.0"
    );
    let mut req = contract(proto::EnsureHumanRequest { name: "new".into() });
    req.metadata_mut()
        .insert("bbs-client-version", "9999.0.0".parse().unwrap());
    raw.ensure_human(req).await.unwrap();
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    assert_eq!(
        health
            .check(HealthCheckRequest {
                service: "bbs.v2.Bbs".into()
            })
            .await
            .unwrap()
            .into_inner()
            .status(),
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn legacy_service_is_unimplemented_without_mutation() {
    let server = Server::start("legacy-protocol");
    let channel = Channel::from_shared(format!("http://{}", server.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let response: Result<tonic::Response<proto::RegisterResponse>, _> = grpc
        .unary(
            as_human(
                "will",
                proto::RegisterRequest {
                    name: Some("Legacy".into()),
                    ..Default::default()
                },
            ),
            tonic::codegen::http::uri::PathAndQuery::from_static("/bbs.v1.Bbs/Register"),
            tonic_prost::ProstCodec::default(),
        )
        .await;
    assert_eq!(response.unwrap_err().code(), tonic::Code::Unimplemented);
    let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM agents", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

async fn register(client: &mut Client) -> proto::RegisterResponse {
    register_as(client, "will", None).await
}

async fn register_as(
    client: &mut Client,
    human: &str,
    name: Option<&str>,
) -> proto::RegisterResponse {
    client
        .ensure_human(proto::EnsureHumanRequest { name: human.into() })
        .await
        .unwrap();
    let req = proto::RegisterRequest {
        name: name.map(str::to_string),
        note: Some("testing".into()),
        pwd: "/tmp".into(),
        hostname: "test".into(),
    };
    client
        .register(as_human(human, req))
        .await
        .unwrap()
        .into_inner()
}

#[tokio::test]
async fn serve_round_trip_and_wait_for_mail() {
    let server = Server::start("round-trip");
    let mut client = server.client().await;

    let created = client
        .ensure_human(proto::EnsureHumanRequest {
            name: "will".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .created;
    assert!(created);
    let group = proto::GroupRequest {
        group: "@acme".into(),
    };
    let register = proto::GroupRegisterRequest {
        group: "@acme".into(),
        description: Some("Acme work".into()),
    };
    client
        .group_register(as_human("will", register))
        .await
        .unwrap();

    let a = register_as(&mut client, "will", Some("Jeffrey")).await;
    let b = register_as(&mut client, "ada", None).await;
    assert_eq!(a.name, "Jeffrey");
    for agent in [&a, &b] {
        let joined = client
            .group_join(as_agent(&agent.token, group.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((joined.group.as_str(), joined.joined), ("acme", true));
    }
    let groups = client
        .group_list(as_agent(&b.token, proto::Empty {}))
        .await
        .unwrap()
        .into_inner()
        .groups;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].description.as_deref(), Some("Acme work"));
    let me = client
        .whoami(as_agent(&b.token, proto::Empty {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.name, b.name);
    let agents = client
        .list_agents(as_agent(&b.token, proto::ListAgentsRequest { all: false }))
        .await
        .unwrap()
        .into_inner()
        .agents;
    let sender = agents.iter().find(|agent| agent.name == a.name).unwrap();
    assert_eq!(sender.name, "Jeffrey");
    assert_eq!(sender.owner.as_deref(), Some("will"));

    // Errors carry the service's message text.
    let err = client
        .send(as_agent(
            &a.token,
            proto::SendRequest {
                to: "will".into(),
                body: "hi".into(),
            },
        ))
        .await
        .unwrap_err();
    assert!(
        err.message().contains("agents cannot message humans"),
        "{err}"
    );

    // B waits; a second waiter on the same inbox is refused.
    let mut waiter = client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap()
        .into_inner();
    let err = client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap_err();
    assert!(err.message().contains("already running"), "{err}");
    let early = tokio::time::timeout(Duration::from_millis(300), waiter.message()).await;
    assert!(early.is_err(), "waiter returned before any mail was sent");

    let sent = client
        .send(as_agent(
            &a.token,
            proto::SendRequest {
                to: b.name.clone(),
                body: "wake up".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(sent.to, b.name);

    let got = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("waiter woke")
        .unwrap()
        .expect("one message");
    assert_eq!(got.body, "wake up");
    assert_eq!(got.from_name, a.name);
    assert_eq!(got.from_owner.as_deref(), Some("will"));
    assert_eq!(got.reader_owner.as_deref(), Some("ada"));
    assert_eq!(got.to_name.as_deref(), Some(b.name.as_str()));
    let end = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("stream ends")
        .unwrap();
    assert!(end.is_none(), "stream should end after delivering");

    // The mail was taken; the slot is free again.
    let unread = client
        .read(as_agent(&b.token, proto::ReadRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert!(unread.messages.is_empty());
    client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap();
}

#[tokio::test]
async fn waiter_slot_is_released_on_disconnect_and_retirement_ends_wait() {
    let server = Server::start("slot");
    let mut client = server.client().await;
    let a = register(&mut client).await;

    let waiter = client
        .wait_for_mail(as_agent(&a.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap()
        .into_inner();
    drop(waiter);
    // The server notices the dropped stream and frees the slot.
    let mut waiter = None;
    for _ in 0..50 {
        match client
            .wait_for_mail(as_agent(&a.token, proto::WaitRequest { limit: None }))
            .await
        {
            Ok(w) => {
                waiter = Some(w.into_inner());
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let mut waiter = waiter.expect("slot released after disconnect");

    client
        .bye(as_agent(&a.token, proto::Empty {}))
        .await
        .unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("waiter ended")
        .unwrap_err();
    assert!(err.message().contains("retired while waiting"), "{err}");
}

#[tokio::test]
async fn watch_feed_snapshots_on_change_for_humans_only() {
    let server = Server::start("feed");
    let mut client = server.client().await;
    client
        .ensure_human(proto::EnsureHumanRequest {
            name: "will".into(),
        })
        .await
        .unwrap();
    let a = register(&mut client).await;

    let err = client
        .watch_feed(as_agent(
            &a.token,
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            },
        ))
        .await
        .unwrap_err();
    assert!(err.message().contains("humans only"), "{err}");

    let mut feed = client
        .watch_feed(as_human(
            "will",
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let first = feed.message().await.unwrap().unwrap();
    assert!(first.messages.is_empty());

    client
        .send(as_human(
            "will",
            proto::SendRequest {
                to: a.name.clone(),
                body: "hello".into(),
            },
        ))
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), feed.message())
        .await
        .expect("feed updated")
        .unwrap()
        .unwrap();
    assert_eq!(next.messages.len(), 1);
    assert_eq!(next.messages[0].to_name.as_deref(), Some(a.name.as_str()));
    assert_eq!(next.messages[0].read_at, None);
}

#[tokio::test]
async fn feed_filters_are_advertised_and_applied_server_side() {
    let server = Server::start("feed-filter");
    let mut client = server.client().await;
    let info = client
        .server_info(proto::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert!(info.capabilities.iter().any(|c| c == "feed-filter.v1"));
    client
        .ensure_human(proto::EnsureHumanRequest {
            name: "will".into(),
        })
        .await
        .unwrap();
    let a = register(&mut client).await;
    for body in ["one", "two", "three"] {
        client
            .send(as_human(
                "will",
                proto::SendRequest {
                    to: a.name.clone(),
                    body: body.into(),
                },
            ))
            .await
            .unwrap();
    }
    let mut req = contract(proto::FeedRequest {
        backlog: 2,
        from_human: Some("will".into()),
        mine: true,
        ..Default::default()
    });
    req.metadata_mut()
        .insert("bbs-human", "will".parse().unwrap());
    req.metadata_mut().insert(
        "bbs-required-capabilities",
        "owner-attribution.v1,feed-filter.v1".parse().unwrap(),
    );
    let mut raw = server.raw_client().await;
    let first = raw
        .watch_feed(req)
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();
    let bodies: Vec<_> = first.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["two", "three"]);

    // The window is bounded on the server, whatever a client asks for.
    for backlog in [0, 1001, u32::MAX] {
        let err = client
            .watch_feed(as_human(
                "will",
                proto::FeedRequest {
                    backlog,
                    ..Default::default()
                },
            ))
            .await
            .unwrap_err();
        assert!(
            err.message().contains(&format!(
                "feed window must be 1 to 1000 messages, not {backlog}"
            )),
            "{err}"
        );
    }

    // Cursor mode: oldest first after the cursor, in batches, then live.
    let mut tail = client
        .watch_feed(as_human(
            "will",
            proto::FeedRequest {
                backlog: 2,
                after_id: Some(first.messages[0].id - 2),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut bodies = Vec::new();
    while bodies.len() < 3 {
        let batch = tokio::time::timeout(Duration::from_secs(5), tail.message())
            .await
            .expect("cursor batch")
            .unwrap()
            .unwrap();
        assert!(!batch.messages.is_empty() && batch.messages.len() <= 2);
        bodies.extend(batch.messages.into_iter().map(|m| m.body));
    }
    assert_eq!(bodies, ["one", "two", "three"]);
    client
        .send(as_human(
            "will",
            proto::SendRequest {
                to: a.name.clone(),
                body: "four".into(),
            },
        ))
        .await
        .unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(5), tail.message())
        .await
        .expect("live batch")
        .unwrap()
        .unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].body, "four");
}

/// A server from before `feed-filter.v1`: it enforces required capabilities
/// against its own list, as every bbs.v2 server must, and records what it saw.
#[derive(Clone, Default)]
struct LegacyServer(std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>);

impl tonic::server::NamedService for LegacyServer {
    const NAME: &'static str = "bbs.v2.Bbs";
}

impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for LegacyServer {
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
        const LEGACY: &[&str] = &[
            "named-agents.v1",
            "owner-attribution.v1",
            "human-token-sessions.v1",
            "group-archive.v1",
            "group-description.v1",
        ];
        let required = req
            .headers()
            .get("bbs-required-capabilities")
            .map(|v| v.to_str().unwrap().to_string());
        self.0.lock().unwrap().push(required.clone());
        let missing = required
            .iter()
            .flat_map(|r| r.split(','))
            .find(|c| !LEGACY.contains(c))
            .map(str::to_string);
        Box::pin(async move {
            Ok(match missing {
                Some(c) => tonic::Status::failed_precondition(format!(
                    "incompatible BBS server: required capability {c} is unavailable; upgrade the server or use a compatible client"
                )),
                None => tonic::Status::unavailable("legacy server accepted the request"),
            }
            .into_http())
        })
    }
}

#[tokio::test]
async fn filtered_view_refuses_a_server_without_feed_filter() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let legacy = LegacyServer::default();
    let seen = legacy.0.clone();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(legacy)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let home = std::env::temp_dir().join(format!("bbs-serve-{}-legacy", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let view = |args: &'static [&'static str]| {
        let (home, addr) = (home.clone(), addr.clone());
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_bbs"))
                .arg("view")
                .args(args)
                .env("HOME", &home)
                .env("USER", "will")
                .env("BBS_SERVER", &addr)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        })
    };
    for args in [
        // Following JSON reads by cursor, which an older server would ignore.
        &["--json"][..],
        &["--json", "--group", "acme"],
        &["--no-follow", "--from", "@will"],
        &["--json", "--mine"],
        &["--json", "--since", "2h"],
        &["--no-follow", "--until", "2026-01-01T00:00:00Z"],
    ] {
        let out = view(args).await.unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "no unfiltered fallback for {args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("required capability feed-filter.v1 is unavailable"),
            "{args:?}: {stderr}"
        );
    }
    // --limit maps onto the backlog every server honours, so it needs nothing new.
    let out = view(&["--json", "--no-follow", "--limit", "5"])
        .await
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("legacy server accepted"), "{stderr}");
    server.abort();
    let _ = std::fs::remove_dir_all(&home);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 7, "one request per run, no retry or fallback");
    assert!(
        seen[..6]
            .iter()
            .all(|r| r.as_deref() == Some("owner-attribution.v1,feed-filter.v1"))
    );
    assert_eq!(seen[6].as_deref(), Some("owner-attribution.v1"));
}

#[tokio::test]
async fn health_reports_initialized_services_without_a_human() {
    let server = Server::start("health");
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    for service in ["", "bbs.v2.Bbs"] {
        let result = health
            .check(HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(result.status(), ServingStatus::Serving);
    }
    let error = health
        .check(HealthCheckRequest {
            service: "missing".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
}

#[cfg(unix)]
fn signal(server: &Server, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &server.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
async fn expect_clean_exit(server: &mut Server, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server finished shutdown within its grace period");
}

#[cfg(unix)]
#[tokio::test]
async fn termination_closes_mail_and_feed_streams() {
    for name in ["-TERM", "-INT"] {
        let mut server = Server::start(name);
        let mut client = server.client().await;
        client
            .ensure_human(proto::EnsureHumanRequest {
                name: "will".into(),
            })
            .await
            .unwrap();
        let agent = register(&mut client).await;
        let mut mail = client
            .wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
            .await
            .unwrap()
            .into_inner();
        let mut feed = client
            .watch_feed(as_human(
                "will",
                proto::FeedRequest {
                    backlog: 10,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        feed.message().await.unwrap().unwrap();
        signal(&server, name);
        expect_clean_exit(&mut server, Duration::from_secs(3)).await;
        assert_eq!(
            mail.message().await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        assert_eq!(
            feed.message().await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
        let integrity: String = db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn health_watchers_cannot_prevent_shutdown() {
    let mut server = Server::start("health-watch-shutdown");
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    let mut watch = health
        .watch(HealthCheckRequest {
            service: "bbs.v2.Bbs".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        watch.message().await.unwrap().unwrap().status(),
        ServingStatus::Serving
    );
    signal(&server, "-TERM");
    let last = tokio::time::timeout(Duration::from_secs(2), watch.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(last.status(), ServingStatus::NotServing);
    // Keep the health stream alive: the server must enforce its drain deadline.
    expect_clean_exit(&mut server, Duration::from_secs(10)).await;
    drop(watch);
}

fn credential<T>(token: &str, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-human-token", token.parse().unwrap());
    req
}

impl Server {
    fn admin(&self, action: &str, name: &str) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["admin", "human", action, name, "--state-dir"])
            .arg(self.home.join(".local/state/bbs"))
            .env_remove("HOME")
            .env("BBS_SERVER", "invalid:1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}

async fn protected_register(
    client: &mut Client,
    token: &str,
    name: &str,
) -> proto::RegisterResponse {
    client
        .register(credential(
            token,
            proto::RegisterRequest {
                name: Some(name.into()),
                note: None,
                pwd: "/tmp".into(),
                hostname: "test".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
}

#[tokio::test]
async fn protected_identity_and_session_ownership() {
    let server = Server::start_mode("protected-ownership", true);
    let mut client = server.client().await;
    assert!(
        client
            .server_info(proto::Empty {})
            .await
            .unwrap()
            .into_inner()
            .require_human_token
    );
    assert_eq!(
        client
            .ensure_human(proto::EnsureHumanRequest {
                name: "outsider".into()
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(
        client
            .whoami(as_human("Ada", proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let ada = server.admin("add", "Ada");
    let bob = server.admin("add", "Bob");
    assert_eq!(ada.len(), 26);
    assert_eq!(
        client
            .whoami(credential(&ada, proto::Empty {}))
            .await
            .unwrap()
            .into_inner()
            .name,
        "Ada"
    );
    let mut forged = credential(&ada, proto::Empty {});
    forged
        .metadata_mut()
        .insert("bbs-human", "Bob".parse().unwrap());
    assert_eq!(
        client.whoami(forged).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let a = protected_register(&mut client, &ada, "Alpha").await;
    let b = protected_register(&mut client, &bob, "Bravo").await;
    assert_eq!(a.token.len(), 26);
    let bind = |token: &str| proto::BindSessionRequest {
        harness: "test".into(),
        session_id: "shared".into(),
        token: token.into(),
    };
    let lookup = || proto::SessionAgentRequest {
        harness: "test".into(),
        session_id: "shared".into(),
    };
    client
        .bind_session(credential(&ada, bind(&a.token)))
        .await
        .unwrap();
    assert_eq!(
        client
            .session_agent(credential(&ada, lookup()))
            .await
            .unwrap()
            .into_inner()
            .token,
        a.token
    );
    assert_eq!(
        client.session_agent(lookup()).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let foreign = client
        .session_agent(credential(&bob, lookup()))
        .await
        .unwrap()
        .into_inner();
    assert!(foreign.agent.is_none() && foreign.token.is_empty());
    assert!(
        client
            .bind_session(credential(&bob, bind(&a.token)))
            .await
            .is_err()
    );
    assert!(
        client
            .bind_session(credential(&bob, bind(&b.token)))
            .await
            .is_err()
    );
    assert_eq!(
        client
            .whoami(as_agent(&a.token, proto::Empty {}))
            .await
            .unwrap()
            .into_inner()
            .name,
        "Alpha"
    );
    let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
    let hash: String = db.query_row("SELECT token_hash FROM human_tokens JOIN humans ON humans.id = human_tokens.human_id WHERE humans.name = 'Ada'", [], |r| r.get(0)).unwrap();
    assert_eq!(hash.len(), 64);
    assert!(!hash.contains(&ada));
    // Required mode rejects both old short tokens and agents with no owner.
    db.execute(
        "UPDATE agents SET token = 'abcdefg' WHERE name = 'Alpha'",
        [],
    )
    .unwrap();
    assert_eq!(
        client
            .whoami(as_agent("abcdefg", proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert!(
        client
            .session_agent(credential(&ada, lookup()))
            .await
            .unwrap()
            .into_inner()
            .token
            .is_empty()
    );
    db.execute(
        "DELETE FROM agent_owners WHERE agent_id = (SELECT id FROM agents WHERE name = 'Bravo')",
        [],
    )
    .unwrap();
    assert_eq!(
        client
            .whoami(as_agent(&b.token, proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
}

#[tokio::test]
async fn live_rotation_and_revocation_end_idle_streams() {
    let server = Server::start_mode("protected-revoke", true);
    let mut client = server.client().await;
    let old = server.admin("add", "Ada");
    let agent = protected_register(&mut client, &old, "Alpha").await;
    let mut old_feed = client
        .watch_feed(credential(
            &old,
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    old_feed.message().await.unwrap().unwrap();
    let new = server.admin("rotate", "Ada");
    assert_ne!(old, new);
    assert_eq!(
        client
            .whoami(credential(&old, proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    client
        .whoami(credential(&new, proto::Empty {}))
        .await
        .unwrap();
    client
        .whoami(as_agent(&agent.token, proto::Empty {}))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(7), old_feed.message())
            .await
            .unwrap()
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let mut mail = client
        .wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap()
        .into_inner();
    let mut feed = client
        .watch_feed(credential(
            &new,
            proto::FeedRequest {
                backlog: 10,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    feed.message().await.unwrap().unwrap();
    server.admin("revoke", "Ada");
    assert_eq!(
        client
            .whoami(credential(&new, proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        client
            .whoami(as_agent(&agent.token, proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let (mail_end, feed_end) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(7), mail.message()),
        tokio::time::timeout(Duration::from_secs(7), feed.message())
    );
    assert_eq!(
        mail_end.unwrap().unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        feed_end.unwrap().unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let restored = server.admin("rotate", "Ada");
    client
        .whoami(credential(&restored, proto::Empty {}))
        .await
        .unwrap();
    client
        .whoami(as_agent(&agent.token, proto::Empty {}))
        .await
        .unwrap();
}

#[tokio::test]
async fn authentication_failure_budget_preserves_valid_callers_and_health() {
    let server = Server::start_mode("auth-rate", true);
    let mut client = server.client().await;
    for _ in 0..20 {
        assert_eq!(
            client
                .whoami(credential("incorrect", proto::Empty {}))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );
    }
    assert_eq!(
        client
            .whoami(credential("incorrect", proto::Empty {}))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::ResourceExhausted
    );
    let valid = server.admin("add", "Ada");
    client
        .whoami(credential(&valid, proto::Empty {}))
        .await
        .unwrap();
    client.server_info(proto::Empty {}).await.unwrap();
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    assert_eq!(
        health
            .check(HealthCheckRequest {
                service: "bbs.v2.Bbs".into()
            })
            .await
            .unwrap()
            .into_inner()
            .status(),
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn enrollment_validates_before_saving_and_binds_credentials_to_server() {
    use std::io::Write;
    let server = Server::start_mode("auth-enrollment", true);
    let other = Server::start_mode("auth-other", true);
    let token = server.admin("add", "Ada");
    let home = tempfile::tempdir().unwrap();
    let enroll = |token: &str, extra: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["init", "--server", &server.addr, "--human-token-stdin"])
            .args(extra)
            .env("HOME", home.path())
            .env("USER", "unrelated")
            .env_remove("BBS_SERVER")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
        child.wait_with_output().unwrap()
    };
    let failed = enroll("invalid-secret", &[]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("invalid-secret"));
    let path = home.path().join(".config/bbs/config.toml");
    assert!(!path.exists());
    let ok = enroll(&token, &[]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("human = \"Ada\""));
    assert!(saved.contains(&token));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(!enroll(&token, &["--human", "Bob"]).status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    // Even when the alternate server recognizes the same credential, it must
    // not receive a token cached for the first server.
    let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
    let hash: String = db
        .query_row("SELECT token_hash FROM human_tokens", [], |r| r.get(0))
        .unwrap();
    other.admin("add", "Ada");
    let other_db = rusqlite::Connection::open(other.home.join(".local/state/bbs/bbs.db")).unwrap();
    other_db
        .execute("UPDATE human_tokens SET token_hash = ?1", [hash])
        .unwrap();
    let overridden = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .arg("whoami")
        .env("HOME", home.path())
        .env("BBS_SERVER", &other.addr)
        .output()
        .unwrap();
    assert!(!overridden.status.success());
    assert!(String::from_utf8_lossy(&overridden.stderr).contains("human token required"));
}
