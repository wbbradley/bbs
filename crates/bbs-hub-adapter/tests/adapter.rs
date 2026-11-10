//! The adapter end to end, in process: a real BBS server (`bbs serve` with
//! its integration listener, on a board in a temporary directory) on one
//! side, and a scripted Hub relay on the other, which the test drives frame
//! by frame and signs for as Hub's `hub` namespace would.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use bbs_core::hub_proto::msgproto_v1 as mp;
use bbs_core::{backend, db, proto, service};
use bbs_hub_adapter::store::{Binding, Direction, Store};
use bbs_hub_adapter::{Adapter, Options, Settings, now_micros, sign, verify};
use ed25519_dalek::SigningKey;
use mp::federation_frame::Payload as F;
use prost::Message as _;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(500);
const TEXT: &str = "text/plain; charset=utf-8";

// Public test keys: Hub's namespace authority and installation, and this
// BBS installation's.
fn key(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
}
const HUB_AUTHORITY: u8 = 1;
const HUB_INSTALLATION: u8 = 2;
const BBS_AUTHORITY: u8 = 3;
const BBS_INSTALLATION: u8 = 4;

fn credential(authority: u8, namespace: &str, installation: &str, subject: u8) -> mp::SignedRecord {
    sign::sign(
        &key(authority),
        mp::RecordKind::InstallationCredential,
        mp::InstallationCredential {
            namespace: namespace.into(),
            installation_id: installation.into(),
            public_key: key(subject).verifying_key().to_bytes().to_vec(),
            permissions: vec![
                mp::InstallationPermission::AssertSenders as i32,
                mp::InstallationPermission::HostNamespace as i32,
            ],
        }
        .encode_to_vec(),
    )
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    while !ok() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---- BBS --------------------------------------------------------------------

/// A BBS server in namespace `bbs` with human `will`, his agent `scout`
/// (entity `will/scout`) and integration `hub`, publishing.
struct Bbs {
    dir: tempfile::TempDir,
    main: SocketAddr,
    integration: SocketAddr,
    token: String,
    agent: String,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Bbs {
    async fn start() -> Bbs {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("bbs.db")).unwrap();
        db::migrate_in(&mut conn, Some("bbs")).unwrap();
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let agent = service::register(
            &mut conn,
            &service::Caller {
                human: Some("will".into()),
                ..Default::default()
            },
            proto::RegisterRequest {
                name: Some("scout".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let token = service::integration::admin_add(&mut conn, "hub").unwrap();
        service::integration::admin_publish(&mut conn, "hub", true).unwrap();
        drop(conn);
        // Ports are picked free, then bound by the server: retry if another
        // test took one in between.
        for _ in 0..5 {
            let (main, integration) = {
                let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                (a.local_addr().unwrap(), b.local_addr().unwrap())
            };
            let path = dir.path().to_path_buf();
            let task = tokio::spawn(async move {
                bbs_server::serve::run(
                    main,
                    None,
                    Some(integration),
                    &path,
                    Some("bbs"),
                    false,
                    bbs_core::protocol::Policy::default(),
                    service::TimerLimits::default(),
                )
                .await
            });
            let start = tokio::time::Instant::now();
            loop {
                if tokio::net::TcpStream::connect(integration).await.is_ok() {
                    return Bbs {
                        dir,
                        main,
                        integration,
                        token,
                        agent,
                        task,
                    };
                }
                if task.is_finished() {
                    break;
                }
                assert!(start.elapsed() < WAIT, "BBS did not start");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        panic!("BBS could not bind its ports");
    }

    fn db(&self) -> rusqlite::Connection {
        db::open(&self.dir.path().join("bbs.db")).unwrap()
    }

    /// Send as agent `scout`; returns the message's ID.
    async fn send(&self, to: &str, body: &str) -> i64 {
        let remote = backend::Remote::connect(
            &self.main.to_string(),
            service::Caller {
                token: Some(self.agent.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        backend::Backend::Remote(remote)
            .send(proto::SendRequest {
                to: to.into(),
                body: body.into(),
                control: None,
            })
            .await
            .unwrap()
            .id
            .unwrap()
    }

    fn outbox(&self, message: i64) -> (String, Option<String>) {
        db::integration::outbox_state(&self.db(), message)
            .unwrap()
            .unwrap_or_default()
    }

    /// The stored bytes of the imported message with origin `hub:id`.
    fn imported(&self, id: &str) -> Option<Vec<u8>> {
        db::integration::origin_bytes(&self.db(), "hub", id)
            .unwrap()
            .flatten()
    }
}

impl Drop for Bbs {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---- the relay -----------------------------------------------------------------

type Conn = (
    Streaming<mp::FederationFrame>,
    mpsc::Sender<Result<mp::FederationFrame, Status>>,
);

struct FakeRelay(mpsc::Sender<Conn>);

#[tonic::async_trait]
impl mp::federation_server::Federation for FakeRelay {
    type ConnectStream = ReceiverStream<Result<mp::FederationFrame, Status>>;

    async fn connect(
        &self,
        req: Request<Streaming<mp::FederationFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let (tx, rx) = mpsc::channel(64);
        self.0
            .send((req.into_inner(), tx))
            .await
            .map_err(|_| Status::unavailable("the test has finished"))?;
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// A scripted relay for namespace `hub`, trusting `hub` and `bbs`.
struct Relay {
    addr: SocketAddr,
    conns: mpsc::Receiver<Conn>,
}

/// One adapter's stream at the relay.
struct Peer {
    rx: Streaming<mp::FederationFrame>,
    tx: mpsc::Sender<Result<mp::FederationFrame, Status>>,
    hello: mp::PeerHello,
}

fn binding(namespace: &str, authority: u8) -> mp::NamespaceBinding {
    mp::NamespaceBinding {
        namespace: namespace.into(),
        authority_public_key: key(authority).verifying_key().to_bytes().to_vec(),
        revision: 1,
        disabled: false,
    }
}

impl Relay {
    async fn start() -> Relay {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, conns) = mpsc::channel(4);
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(mp::federation_server::FederationServer::new(FakeRelay(tx)))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        Relay { addr, conns }
    }

    /// Take the next stream through hello, trust and route registration.
    async fn accept(&mut self) -> Peer {
        let mut peer = self.open().await;
        peer.trust().await;
        let claim = peer.route().await;
        peer.route_result(claim, true).await;
        peer
    }

    /// Take the next stream through the hellos.
    async fn open(&mut self) -> Peer {
        let (rx, tx) = timeout(WAIT, self.conns.recv())
            .await
            .expect("the adapter connects")
            .unwrap();
        tx.send(Ok(frame(F::Hello(mp::PeerHello {
            protocol_major: 1,
            installation_id: "hub-1".into(),
            connection_id: "relay-conn".into(),
            build: "fake relay".into(),
            serves_relay: true,
        }))))
        .await
        .unwrap();
        let mut peer = Peer {
            rx,
            tx,
            hello: Default::default(),
        };
        let F::Hello(hello) = peer.next().await else {
            panic!("the first frame is a hello");
        };
        assert_eq!(
            (hello.protocol_major, hello.installation_id.as_str()),
            (1, "bbs-1")
        );
        peer.hello = hello;
        peer
    }
}

fn frame(p: F) -> mp::FederationFrame {
    mp::FederationFrame { payload: Some(p) }
}

impl Peer {
    /// Answer the trust query that opens every stream.
    async fn trust(&mut self) {
        let F::TrustQuery(q) = self.next().await else {
            panic!("trust is refreshed before the route is claimed");
        };
        self.send(F::TrustPage(mp::TrustPage {
            query_id: q.query_id,
            bindings: vec![binding("hub", HUB_AUTHORITY), binding("bbs", BBS_AUTHORITY)],
            ..Default::default()
        }))
        .await;
    }

    /// Read the route claim, checked.
    async fn route(&mut self) -> mp::RouteClaim {
        let F::RegisterRoute(reg) = self.next().await else {
            panic!("then the route is claimed");
        };
        let claim_rec = reg.claim.unwrap();
        sign::verify(&claim_rec, mp::RecordKind::RouteClaim).unwrap();
        let claim = mp::RouteClaim::decode(claim_rec.payload.as_slice()).unwrap();
        assert_eq!(claim.namespace, "bbs");
        assert_eq!(claim.connection_id, self.hello.connection_id);
        assert_eq!(
            reg.credential.unwrap(),
            credential(BBS_AUTHORITY, "bbs", "bbs-1", BBS_INSTALLATION)
        );
        claim
    }

    async fn route_result(&self, claim: mp::RouteClaim, accepted: bool) {
        self.send(F::RouteResult(mp::RouteResult {
            claim_id: claim.claim_id,
            namespace: "bbs".into(),
            accepted,
            error_code: if accepted { "" } else { "route_conflict" }.into(),
            ..Default::default()
        }))
        .await;
    }

    /// The next frame, heartbeats included; None when the stream ends.
    async fn next_any(&mut self) -> Option<F> {
        timeout(WAIT, self.rx.message())
            .await
            .expect("a frame in time")
            .ok()
            .flatten()
            .map(|f| f.payload.expect("a payload"))
    }

    /// The next frame that is not a heartbeat.
    async fn next(&mut self) -> F {
        loop {
            let f = timeout(WAIT, self.rx.message())
                .await
                .expect("a frame in time")
                .expect("the stream is up")
                .expect("the stream is open");
            match f.payload.expect("a payload") {
                F::Heartbeat(_) => continue,
                p => return p,
            }
        }
    }

    /// Any frame but heartbeats within a short while.
    async fn quiet(&mut self) -> Option<F> {
        timeout(QUIET, self.next()).await.ok()
    }

    async fn send(&self, p: F) {
        self.tx.send(Ok(frame(p))).await.unwrap();
    }

    async fn transfer(&self, handoff: &str, envelope: mp::SignedEnvelope) {
        self.send(F::Transfer(mp::Transfer {
            handoff_id: handoff.into(),
            envelope: Some(envelope),
        }))
        .await;
    }

    async fn result(&mut self) -> mp::TransferResult {
        match self.next().await {
            F::TransferResult(r) => r,
            other => panic!("expected a transfer result, got {other:?}"),
        }
    }
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

/// A message from `will@hub` to `to@bbs`.
fn hub_message(id: &str, to: &str, body: &str) -> mp::Message {
    mp::Message {
        id: Some(mp::MessageRef {
            namespace: "hub".into(),
            message_id: id.into(),
        }),
        sender: address("hub", "will"),
        recipient: address("bbs", to),
        authored_at_unix_micros: now_micros(),
        expires_at_unix_micros: Some(now_micros() + 3_600_000_000),
        content_type: TEXT.into(),
        body: body.as_bytes().to_vec(),
        kind: mp::MessageKind::Application as i32,
        ..Default::default()
    }
}

/// Hub's envelope for `message`'s exact bytes.
fn hub_envelope(message: &[u8]) -> mp::SignedEnvelope {
    mp::SignedEnvelope {
        message: Some(sign::sign(
            &key(HUB_INSTALLATION),
            mp::RecordKind::Message,
            message.to_vec(),
        )),
        credential: Some(credential(HUB_AUTHORITY, "hub", "hub-1", HUB_INSTALLATION)),
    }
}

// ---- the adapter -------------------------------------------------------------------

fn options() -> Options {
    Options {
        heartbeat: Duration::from_millis(500),
        peer_timeout: Duration::from_secs(10),
        reply_timeout: Duration::from_secs(5),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(400),
        sweep: Duration::from_millis(200),
        poll: Duration::from_millis(100),
        retention: None,
        drain: Duration::from_secs(2),
    }
}

fn settings(bbs: SocketAddr, relay: SocketAddr, token: &str, namespace: &str) -> Settings {
    Settings {
        namespace: namespace.into(),
        installation_id: "bbs-1".into(),
        integration_id: "hub".into(),
        bbs_endpoint: format!("http://{bbs}"),
        relay_endpoint: format!("http://{relay}"),
        token: token.into(),
        token_file: None,
        key: key(BBS_INSTALLATION),
        credential: credential(BBS_AUTHORITY, "bbs", "bbs-1", BBS_INSTALLATION),
    }
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn start(settings: Settings, store: &std::path::Path) -> Running {
        Running::start_with(settings, store, options())
    }

    fn start_with(settings: Settings, store: &std::path::Path, opts: Options) -> Running {
        let adapter = Adapter::open(settings, store, opts).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(adapter.run(async {
            let _ = stopped.await;
        }));
        Running {
            stop: Some(stop),
            task,
        }
    }

    async fn stop(mut self) -> anyhow::Result<()> {
        let _ = self.stop.take().unwrap().send(());
        timeout(WAIT, &mut self.task)
            .await
            .expect("the adapter stops")
            .unwrap()
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: PathBuf,
    bbs: Bbs,
    relay: Relay,
}

impl World {
    async fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        World {
            store: dir.path().join("adapter.db"),
            _dir: dir,
            bbs: Bbs::start().await,
            relay: Relay::start().await,
        }
    }

    fn adapter(&self) -> Running {
        Running::start(
            settings(
                self.bbs.integration,
                self.relay.addr,
                &self.bbs.token,
                "bbs",
            ),
            &self.store,
        )
    }

    fn store(&self) -> Store {
        Store::open_read_only(&self.store).unwrap()
    }
}

/// A store trusting `bbs`, to verify what the adapter signed.
fn verifier() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("check.db"), 0).unwrap();
    store
        .put_binding(
            &Binding {
                namespace: "bbs".into(),
                authority_key: key(BBS_AUTHORITY).verifying_key().to_bytes().to_vec(),
                revision: 1,
                disabled: false,
            },
            0,
        )
        .unwrap();
    (dir, store)
}

// ---- tests -------------------------------------------------------------------------

/// BBS's remote sends reach the relay signed over BBS's exact frozen bytes,
/// and the relay's receipt, duplicate and rejection come back to BBS.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_sends_are_signed_and_their_results_reach_bbs() {
    let mut w = World::new().await;
    let adapter = w.adapter();
    let mut peer = w.relay.accept().await;
    let (_vdir, check) = verifier();

    let mut handoffs = Vec::new();
    for (n, body) in ["first", "second", "third"].iter().enumerate() {
        let message = w.bbs.send("id:will@hub", body).await;
        let F::Transfer(t) = peer.next().await else {
            panic!("a transfer");
        };
        let envelope = t.envelope.unwrap().encode_to_vec();
        let v = verify::envelope(&check, &envelope).expect("the envelope verifies");
        assert_eq!(v.message.body, body.as_bytes());
        assert_eq!(
            v.message.sender.unwrap().entity.unwrap().entity_id,
            "will/scout"
        );
        // Exactly the bytes BBS froze.
        let stored: Vec<u8> = w
            .bbs
            .db()
            .query_row(
                "SELECT encoded_message FROM messages WHERE id = ?1",
                [message],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v.bytes, stored, "message {n}");
        handoffs.push((message, t.handoff_id));
    }
    let answer = |handoff: &str, disposition: mp::TransferDisposition, code: &str| {
        F::TransferResult(mp::TransferResult {
            handoff_id: handoff.into(),
            disposition: disposition as i32,
            error_code: code.into(),
            detail: "detail from the relay".into(),
        })
    };
    peer.send(answer(
        &handoffs[0].1,
        mp::TransferDisposition::Received,
        "",
    ))
    .await;
    peer.send(answer(
        &handoffs[1].1,
        mp::TransferDisposition::AlreadyKnown,
        "",
    ))
    .await;
    peer.send(answer(
        &handoffs[2].1,
        mp::TransferDisposition::Rejected,
        "recipient_unknown",
    ))
    .await;
    // A result for a handoff never attempted on this stream is ignored.
    peer.send(answer(
        "no-such-handoff",
        mp::TransferDisposition::Rejected,
        "x",
    ))
    .await;
    let bbs = &w.bbs;
    eventually("BBS records the results", || {
        bbs.outbox(handoffs[0].0).0 == "received"
            && bbs.outbox(handoffs[1].0).0 == "received"
            && bbs.outbox(handoffs[2].0).0 == "rejected"
    })
    .await;
    assert_eq!(
        bbs.outbox(handoffs[1].0).1.as_deref(),
        Some("already_known")
    );
    let rejected = bbs.outbox(handoffs[2].0).1.unwrap();
    assert!(rejected.starts_with("recipient_unknown"), "{rejected}");
    // A late, contradicting result changes nothing.
    peer.send(answer(
        &handoffs[0].1,
        mp::TransferDisposition::Rejected,
        "expired",
    ))
    .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(bbs.outbox(handoffs[0].0).0, "received");
    adapter.stop().await.unwrap();
    let counts = w.store().counts(Direction::Outbound).unwrap();
    assert_eq!(
        counts,
        vec![("received".to_string(), 2), ("rejected".to_string(), 1)]
    );
}

/// Hub's messages are verified, offered to BBS, and BBS's receipt,
/// duplicate, conflict and refusal go back to the relay; what the adapter
/// can refuse itself never reaches BBS.
#[tokio::test(flavor = "multi_thread")]
async fn inbound_transfers_are_verified_imported_and_answered() {
    let mut w = World::new().await;
    let adapter = w.adapter();
    let mut peer = w.relay.accept().await;
    let received = mp::TransferDisposition::Received as i32;
    let rejected = mp::TransferDisposition::Rejected as i32;
    let known = mp::TransferDisposition::AlreadyKnown as i32;

    let bytes = hub_message("msg-1", "will/scout", "hello from hub").encode_to_vec();
    peer.transfer("t1", hub_envelope(&bytes)).await;
    let r = peer.result().await;
    assert_eq!(
        (r.handoff_id.as_str(), r.disposition),
        ("t1", received),
        "{r:?}"
    );
    assert_eq!(w.bbs.imported("msg-1").as_deref(), Some(bytes.as_slice()));

    // The same message again: the adapter knows it; BBS is not offered it.
    peer.transfer("t2", hub_envelope(&bytes)).await;
    let r = peer.result().await;
    assert_eq!((r.handoff_id.as_str(), r.disposition), ("t2", known));
    // The same ID with other content.
    let changed = hub_message("msg-1", "will/scout", "changed").encode_to_vec();
    peer.transfer("t3", hub_envelope(&changed)).await;
    let r = peer.result().await;
    assert_eq!(
        (r.disposition, r.error_code.as_str()),
        (rejected, "message_conflict")
    );

    // BBS's own refusal: no such recipient.
    let nobody = hub_message("msg-2", "nobody", "hello?").encode_to_vec();
    peer.transfer("t4", hub_envelope(&nobody)).await;
    let r = peer.result().await;
    assert_eq!(
        (r.handoff_id.as_str(), r.disposition, r.error_code.as_str()),
        ("t4", rejected, "recipient_unknown")
    );

    // A tampered signature, a sender the credential cannot assert, and a
    // message for another namespace are refused here.
    let mut bad = hub_envelope(&hub_message("msg-3", "will/scout", "x").encode_to_vec());
    bad.message.as_mut().unwrap().signature[0] ^= 1;
    peer.transfer("t5", bad).await;
    assert_eq!(peer.result().await.error_code, "invalid_signature");
    let mut forged = hub_message("msg-4", "will/scout", "x");
    forged.sender = address("bbs", "will");
    forged.id.as_mut().unwrap().namespace = "bbs".into();
    peer.transfer("t6", hub_envelope(&forged.encode_to_vec()))
        .await;
    assert_eq!(peer.result().await.error_code, "route_not_authorized");
    let mut elsewhere = hub_message("msg-5", "will/scout", "x");
    elsewhere.recipient = address("lab", "ann");
    peer.transfer("t7", hub_envelope(&elsewhere.encode_to_vec()))
        .await;
    assert_eq!(peer.result().await.error_code, "route_not_authorized");

    // A group's copy: sent by the group, naming the original and its
    // sender; never refused for coming from a group.
    let mut copy = hub_message("msg-6", "will/scout", "to the group");
    copy.sender = address("hub", "release");
    copy.group_copy = Some(mp::GroupCopy {
        original: Some(mp::MessageRef {
            namespace: "hub".into(),
            message_id: "msg-orig".into(),
        }),
        original_sender: address("hub", "ann"),
    });
    let copy = copy.encode_to_vec();
    peer.transfer("t8", hub_envelope(&copy)).await;
    let r = peer.result().await;
    assert_eq!(
        (r.handoff_id.as_str(), r.disposition),
        ("t8", received),
        "{r:?}"
    );
    assert_eq!(w.bbs.imported("msg-6").as_deref(), Some(copy.as_slice()));

    // A reply to BBS's message, to a run BBS does not have: BBS refuses it.
    let mut reply = hub_message("msg-7", "will/scout", "a reply");
    reply.in_reply_to = Some(mp::MessageRef {
        namespace: "bbs".into(),
        message_id: "earlier".into(),
    });
    reply.recipient.as_mut().unwrap().run_id = Some("run-1".into());
    peer.transfer("t9", hub_envelope(&reply.encode_to_vec()))
        .await;
    assert_eq!(peer.result().await.error_code, "recipient_unknown");

    adapter.stop().await.unwrap();
    let counts = w.store().counts(Direction::Inbound).unwrap();
    assert_eq!(
        counts,
        vec![("received".to_string(), 2), ("rejected".to_string(), 2)]
    );
}

/// Work that arrives while the other side is down waits, pending, and goes
/// once it is back.
#[tokio::test(flavor = "multi_thread")]
async fn work_waits_for_the_side_that_is_down() {
    let mut w = World::new().await;
    // No relay connection yet: BBS's send waits in the adapter.
    let adapter = w.adapter();
    let peer_conn = timeout(WAIT, w.relay.conns.recv()).await.unwrap().unwrap();
    drop(peer_conn); // refuse the first stream
    let message = w.bbs.send("id:ann@hub", "while the relay is away").await;
    let store_path = w.store.clone();
    eventually("the adapter takes the send", || {
        Store::open_read_only(&store_path)
            .unwrap()
            .counts(Direction::Outbound)
            .unwrap()
            == vec![("pending".to_string(), 1)]
    })
    .await;
    assert_eq!(w.bbs.outbox(message).0, "sent");
    let mut peer = w.relay.accept().await;
    let F::Transfer(t) = peer.next().await else {
        panic!("the pending send goes once the relay is back");
    };
    peer.send(F::TransferResult(mp::TransferResult {
        handoff_id: t.handoff_id,
        disposition: mp::TransferDisposition::Received as i32,
        ..Default::default()
    }))
    .await;
    let bbs = &w.bbs;
    eventually("BBS records the receipt", || {
        bbs.outbox(message).0 == "received"
    })
    .await;
    adapter.stop().await.unwrap();
}

/// D072 across a restart: a transfer left attempted by a stop is unknown,
/// never sent again; one known unsent goes; one sent is never resent.
#[tokio::test(flavor = "multi_thread")]
async fn an_attempted_send_is_not_replayed_after_a_restart() {
    let mut w = World::new().await;
    let bbs_offline = free_addr();
    let relay = w.relay.addr;
    let settings = || settings(bbs_offline, relay, "unused", "bbs");
    {
        // A previous run: two sends taken from BBS, one claimed for a stream
        // when the process stopped.
        let mut store = Store::open(&w.store, 1).unwrap();
        for n in 1..=2 {
            let message = mp::Message {
                id: Some(mp::MessageRef {
                    namespace: "bbs".into(),
                    message_id: format!("m{n}"),
                }),
                ..hub_message("x", "ann", "x")
            };
            store
                .intake_outbound(
                    &format!("bbs-h{n}"),
                    &format!("h{n}"),
                    message.id.as_ref().unwrap(),
                    "hub",
                    &message.encode_to_vec(),
                    now_micros() + 3_600_000_000,
                    1,
                )
                .unwrap();
        }
        let c = store
            .claim_outbound("old-conn", 2, |m| m.to_vec())
            .unwrap()
            .unwrap();
        assert_eq!(c.handoff_id, "h1");
    }
    let adapter = Running::start(settings(), &w.store);
    let mut peer = w.relay.accept().await;
    let F::Transfer(t) = peer.next().await else {
        panic!("the send known unsent goes");
    };
    assert_eq!(t.handoff_id, "h2");
    assert!(
        peer.quiet().await.is_none(),
        "the attempted send is not replayed"
    );
    let store = w.store();
    assert_eq!(
        store.state(Direction::Outbound, 1).unwrap().unwrap(),
        ("unknown".to_string(), "restart_after_attempt".to_string())
    );
    eventually("the transfer is recorded sent", || {
        w.store().state(Direction::Outbound, 2).unwrap().unwrap().0 == "sent"
    })
    .await;
    adapter.stop().await.unwrap();

    // Restart again: nothing is offered, neither the sent nor the unknown.
    let adapter = Running::start(settings(), &w.store);
    let mut peer = w.relay.accept().await;
    assert!(peer.quiet().await.is_none(), "sent work is never resent");
    // The unknown one may still learn its result, on its own stream only.
    peer.send(F::TransferResult(mp::TransferResult {
        handoff_id: "h1".into(),
        disposition: mp::TransferDisposition::Received as i32,
        ..Default::default()
    }))
    .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        w.store().state(Direction::Outbound, 1).unwrap().unwrap().0,
        "unknown"
    );
    adapter.stop().await.unwrap();
}

/// Imports whose results were lost with a stream are offered again when BBS
/// connects, and BBS imports each once: one a stop left attempted (unknown
/// after the restart) and one sent to an earlier stream that ended before
/// BBS answered. Pending work goes as usual; once all are settled, a new BBS
/// stream offers nothing. (An import still queued when its stream ends is
/// returned to pending by the link; see link.rs's tests.)
#[tokio::test(flavor = "multi_thread")]
async fn imports_lost_with_a_stream_are_offered_again() {
    let mut w = World::new().await;
    {
        let mut store = Store::open(&w.store, 1).unwrap();
        for n in 1..=3 {
            let bytes = hub_message(&format!("msg-{n}"), "will/scout", "hi").encode_to_vec();
            let origin = mp::MessageRef {
                namespace: "hub".into(),
                message_id: format!("msg-{n}"),
            };
            store
                .intake_inbound(
                    &format!("in-{n}"),
                    &format!("t{n}"),
                    "old-conn",
                    &origin,
                    &bytes,
                    b"env",
                    now_micros() + 3_600_000_000,
                    1,
                )
                .unwrap();
        }
        // in-1: attempted when the last run stopped.
        store.claim_inbound(2).unwrap().unwrap();
        // in-2: pending. in-3: sent to a stream that then ended.
        let c = store.claim_inbound(3).unwrap().unwrap();
        assert_eq!(c.handoff_id, "in-2");
        store.requeue(Direction::Inbound, c.id, 3).unwrap();
        store.claim_inbound(4).unwrap().unwrap();
        store.claim_inbound(4).unwrap().unwrap();
        store.mark_sent(Direction::Inbound, 3, 5).unwrap();
    }
    let adapter = w.adapter();
    let _peer = w.relay.accept().await;
    let store_path = w.store.clone();
    eventually("BBS answers all three", || {
        let s = Store::open_read_only(&store_path).unwrap();
        s.counts(Direction::Inbound).unwrap() == vec![("received".to_string(), 3)]
    })
    .await;
    let copies = |n: u32| -> i64 {
        w.bbs
            .db()
            .query_row(
                "SELECT count(*) FROM deliveries d JOIN messages m ON m.id = d.message_id \
                 WHERE m.origin_namespace = 'hub' AND m.origin_message_id = ?1",
                [format!("msg-{n}")],
                |r| r.get(0),
            )
            .unwrap()
    };
    for n in 1..=3 {
        assert_eq!(copies(n), 1, "msg-{n} is delivered once");
    }
    adapter.stop().await.unwrap();

    // A new run and BBS stream: everything is settled, nothing is offered.
    let adapter = w.adapter();
    let _peer = w.relay.accept().await;
    tokio::time::sleep(QUIET).await;
    for n in 1..=3 {
        assert_eq!(copies(n), 1, "msg-{n} still once");
    }
    adapter.stop().await.unwrap();
}

/// Hub's directory queries for this namespace are answered with BBS's
/// answer, signed by this installation and bound to the query; with BBS
/// away the signed answer says the home is offline.
#[tokio::test(flavor = "multi_thread")]
async fn directory_queries_are_answered_and_signed() {
    let mut w = World::new().await;
    let adapter = w.adapter();
    let mut peer = w.relay.accept().await;
    let query = |id: &str, entity: &str| {
        let q = mp::DirectoryQuery {
            query_id: id.into(),
            requester: Some(mp::Requester {
                entity: Some(mp::EntityRef {
                    namespace: "hub".into(),
                    entity_id: "will".into(),
                }),
                run_id: None,
            }),
            namespace: "bbs".into(),
            entity_id: entity.into(),
            deadline_unix_micros: now_micros() + 5_000_000,
            ..Default::default()
        };
        sign::sign(
            &key(HUB_INSTALLATION),
            mp::RecordKind::DirectoryQuery,
            q.encode_to_vec(),
        )
    };
    let ask = |rec: &mp::SignedRecord| {
        F::DirectoryQuery(mp::FederatedDirectoryQuery {
            query: Some(rec.clone()),
            credential: Some(credential(HUB_AUTHORITY, "hub", "hub-1", HUB_INSTALLATION)),
        })
    };
    let answer = |f: F, rec: &mp::SignedRecord| {
        let F::DirectoryAnswer(a) = f else {
            panic!("a signed answer, got {f:?}");
        };
        let signed = a.answer.unwrap();
        sign::verify(&signed, mp::RecordKind::DirectoryAnswer).unwrap();
        assert_eq!(
            signed.signer_public_key,
            key(BBS_INSTALLATION).verifying_key().to_bytes()
        );
        assert_eq!(
            a.credential.unwrap(),
            credential(BBS_AUTHORITY, "bbs", "bbs-1", BBS_INSTALLATION)
        );
        let answer = mp::DirectoryAnswer::decode(signed.payload.as_slice()).unwrap();
        assert_eq!(answer.namespace, "bbs");
        assert_eq!(answer.query_record_hash, sign::hash(rec).unwrap());
        answer.result.unwrap()
    };

    // The adapter must know BBS is connected: wait for its catalog.
    let store_path = w.store.clone();
    eventually("BBS's catalog", || {
        Store::open_read_only(&store_path)
            .unwrap()
            .catalog_revision()
            .unwrap()
            .is_some()
    })
    .await;
    let rec = query("q1", "will/scout");
    peer.send(ask(&rec)).await;
    let result = answer(peer.next().await, &rec);
    assert_eq!(result.query_id, "q1");
    assert!(result.error_code.is_empty(), "{result:?}");
    let entity = result.entities[0].entity.clone().unwrap();
    assert_eq!(
        (entity.namespace.as_str(), entity.entity_id.as_str()),
        ("bbs", "will/scout")
    );

    // A top-level ID answers as its own alias, so `hub send will@bbs`
    // resolves; an agent's name does not.
    for (id, alias, want) in [("q3", "will", vec!["will"]), ("q4", "scout", vec![])] {
        let mut q = mp::DirectoryQuery::decode(query(id, "").payload.as_slice()).unwrap();
        q.alias = alias.into();
        let rec = sign::sign(
            &key(HUB_INSTALLATION),
            mp::RecordKind::DirectoryQuery,
            q.encode_to_vec(),
        );
        peer.send(ask(&rec)).await;
        let result = answer(peer.next().await, &rec);
        assert!(result.error_code.is_empty(), "{result:?}");
        let ids: Vec<_> = result
            .entities
            .iter()
            .map(|e| e.entity.as_ref().unwrap().entity_id.as_str())
            .collect();
        assert_eq!(ids, want, "alias {alias}");
    }

    // A query whose requester the credential cannot assert is refused.
    let mut forged = mp::DirectoryQuery::decode(query("q2", "will").payload.as_slice()).unwrap();
    forged
        .requester
        .as_mut()
        .unwrap()
        .entity
        .as_mut()
        .unwrap()
        .namespace = "lab".into();
    let rec = sign::sign(
        &key(HUB_INSTALLATION),
        mp::RecordKind::DirectoryQuery,
        forged.encode_to_vec(),
    );
    peer.send(ask(&rec)).await;
    let F::DirectoryUnavailable(u) = peer.next().await else {
        panic!("refused");
    };
    assert_eq!(
        (u.query_id.as_str(), u.error_code.as_str()),
        ("q2", "route_not_authorized")
    );

    adapter.stop().await.unwrap();
}

/// With BBS away, a directory query gets a signed answer that says the home
/// is offline, never an empty directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_query_without_bbs_says_the_home_is_offline() {
    let mut w = World::new().await;
    let adapter = Running::start(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
    );
    let mut peer = w.relay.accept().await;
    let q = mp::DirectoryQuery {
        query_id: "q1".into(),
        requester: Some(mp::Requester {
            entity: Some(mp::EntityRef {
                namespace: "hub".into(),
                entity_id: "will".into(),
            }),
            run_id: None,
        }),
        namespace: "bbs".into(),
        deadline_unix_micros: now_micros() + 5_000_000,
        ..Default::default()
    };
    let rec = sign::sign(
        &key(HUB_INSTALLATION),
        mp::RecordKind::DirectoryQuery,
        q.encode_to_vec(),
    );
    peer.send(F::DirectoryQuery(mp::FederatedDirectoryQuery {
        query: Some(rec.clone()),
        credential: Some(credential(HUB_AUTHORITY, "hub", "hub-1", HUB_INSTALLATION)),
    }))
    .await;
    let F::DirectoryAnswer(a) = peer.next().await else {
        panic!("a signed answer");
    };
    let signed = a.answer.unwrap();
    sign::verify(&signed, mp::RecordKind::DirectoryAnswer).unwrap();
    let answer = mp::DirectoryAnswer::decode(signed.payload.as_slice()).unwrap();
    assert_eq!(answer.query_record_hash, sign::hash(&rec).unwrap());
    let result = answer.result.unwrap();
    assert_eq!(
        (result.query_id.as_str(), result.error_code.as_str()),
        ("q1", "home_offline")
    );
    assert!(result.entities.is_empty());
    adapter.stop().await.unwrap();
}

/// An adapter configured for another namespace than BBS's stops at once:
/// it would sign BBS's messages for the wrong namespace.
#[tokio::test(flavor = "multi_thread")]
async fn a_bbs_in_another_namespace_stops_the_adapter() {
    let w = World::new().await;
    let mut s = settings(w.bbs.integration, w.relay.addr, &w.bbs.token, "bbs");
    s.namespace = "work".into();
    s.credential = credential(BBS_AUTHORITY, "work", "bbs-1", BBS_INSTALLATION);
    let adapter = Adapter::open(s, &w.store, options()).unwrap();
    let err = timeout(WAIT, adapter.run(std::future::pending()))
        .await
        .expect("the adapter stops by itself")
        .unwrap_err();
    assert!(err.to_string().contains("namespace"), "{err}");
}

// ---- relay stream liveness, queues and shutdown ---------------------------------

/// `n` remote sends as BBS would have handed them off, stored before the
/// adapter starts (BBS itself stays offline in these tests).
fn stored_sends(store: &std::path::Path, n: u32) {
    stored_sends_of(store, n, 1);
}

/// As [`stored_sends`], with `body` bytes of body each.
fn stored_sends_of(store: &std::path::Path, n: u32, body: usize) {
    let store = Store::open(store, 1).unwrap();
    for i in 1..=n {
        let message = mp::Message {
            id: Some(mp::MessageRef {
                namespace: "bbs".into(),
                message_id: format!("m{i}"),
            }),
            sender: address("bbs", "will/scout"),
            recipient: address("hub", "ann"),
            body: vec![b'x'; body],
            ..hub_message("x", "ann", "x")
        };
        store
            .intake_outbound(
                &format!("bbs-h{i}"),
                &format!("h{i}"),
                message.id.as_ref().unwrap(),
                "hub",
                &message.encode_to_vec(),
                now_micros() + 3_600_000_000,
                1,
            )
            .unwrap();
    }
}

fn quick(peer_timeout: Duration) -> Options {
    Options {
        heartbeat: Duration::from_millis(100),
        peer_timeout,
        reply_timeout: Duration::from_millis(500),
        ..options()
    }
}

/// Heartbeats go out on schedule, and a relay that sends nothing for the
/// peer timeout is cut; the adapter reconnects.
#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_flow_and_a_silent_relay_is_cut() {
    let mut w = World::new().await;
    let adapter = Running::start_with(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
        quick(Duration::from_millis(800)),
    );
    let mut peer = w.relay.accept().await;
    let mut beats = 0;
    let start = tokio::time::Instant::now();
    while let Some(f) = peer.next_any().await {
        if matches!(f, F::Heartbeat(_)) {
            beats += 1;
        }
    }
    // The stream ended: the adapter cut the silent relay.
    assert!(beats >= 3, "{beats} heartbeats");
    assert!(start.elapsed() < Duration::from_secs(5), "cut in time");
    let _again = w.relay.accept().await;
    adapter.stop().await.unwrap();
}

/// A relay that keeps sending heartbeats but stops reading is cut once
/// frames have waited the peer timeout (Hub's watch does the same). The
/// transfers its transport never took go on the next stream; none taken on
/// the first is sent again.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_stops_reading_is_cut_and_untaken_transfers_go_next_time() {
    const SENDS: u32 = 100;
    let mut w = World::new().await;
    stored_sends(&w.store, SENDS);
    let adapter = Running::start_with(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
        quick(Duration::from_millis(1500)),
    );
    let mut first = w.relay.open().await;
    let conn1 = first.hello.connection_id.clone();
    first.trust().await;
    let claim = first.route().await;
    // Before the route is accepted (so before any transfer is queued),
    // provoke results bigger than the flow-control window, then stop
    // reading for good while still heartbeating.
    for i in 0..1200 {
        first
            .send(F::Transfer(mp::Transfer {
                handoff_id: format!("{i}{}", "!".repeat(3000)),
                envelope: None,
            }))
            .await;
    }
    first.route_result(claim, true).await;
    let beating = tokio::spawn({
        let tx = first.tx.clone();
        async move {
            let mut nonce = 0;
            loop {
                nonce += 1;
                if tx
                    .send(Ok(frame(F::Heartbeat(mp::Heartbeat { nonce }))))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    });
    let mut second = timeout(Duration::from_secs(15), w.relay.accept())
        .await
        .expect("the adapter cut the stream and reconnected");
    beating.abort();
    drop(first);
    let mut again = Vec::new();
    while let Some(f) = second.quiet().await {
        if let F::Transfer(t) = f {
            again.push(t.handoff_id);
        }
    }
    adapter.stop().await.unwrap();
    let store = w.store();
    let mut sent_first = 0;
    let mut sent_second = 0;
    for id in 1..=SENDS as i64 {
        let state = store.state(Direction::Outbound, id).unwrap().unwrap().0;
        assert_eq!(state, "sent", "send {id}");
    }
    let conn_of: Vec<String> = (1..=SENDS)
        .map(|i| {
            rusqlite::Connection::open(&w.store)
                .unwrap()
                .query_row(
                    "SELECT connection_id FROM outbound WHERE handoff_id = ?1",
                    [format!("h{i}")],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .collect();
    for (i, conn) in conn_of.iter().enumerate() {
        let handoff = format!("h{}", i + 1);
        if *conn == conn1 {
            sent_first += 1;
            assert!(!again.contains(&handoff), "{handoff} was sent twice");
        } else {
            sent_second += 1;
            assert!(
                again.contains(&handoff),
                "{handoff} went on the second stream"
            );
        }
    }
    assert_eq!(again.len(), sent_second, "each once");
    assert!(sent_second > 0, "some transfers waited for the next stream");
    assert_eq!(sent_first + sent_second, SENDS as usize);
}

/// Heartbeats go ahead of a full transfer queue: while the relay reads
/// slowly through many large transfers (so flow control paces the
/// transport and the queue stays full), heartbeats keep arriving among
/// them.
#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_go_ahead_of_a_full_transfer_queue() {
    const SENDS: u32 = 150;
    let mut w = World::new().await;
    stored_sends_of(&w.store, SENDS, 50_000);
    let adapter = Running::start_with(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
        quick(Duration::from_secs(10)),
    );
    let mut peer = w.relay.accept().await;
    let mut transfers = 0;
    let mut beats_before_last = 0;
    while transfers < SENDS {
        match peer.next_any().await.expect("the stream stays up") {
            F::Transfer(_) => {
                transfers += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            F::Heartbeat(_) => beats_before_last += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    // 150 transfers at 10 ms take 1.5 s, most of it with the relay's window
    // full: about 15 heartbeat intervals.
    assert!(beats_before_last >= 5, "{beats_before_last} heartbeats");
    adapter.stop().await.unwrap();
}

/// A trust change is refreshed until it applies: a refresh the relay does
/// not answer is retried.
#[tokio::test(flavor = "multi_thread")]
async fn trust_changes_are_retried_until_applied() {
    let mut w = World::new().await;
    let adapter = Running::start_with(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
        quick(Duration::from_secs(10)),
    );
    let mut peer = w.relay.accept().await;
    peer.send(F::TrustChanged(mp::TrustChanged {
        namespace: "lab".into(),
    }))
    .await;
    let F::TrustQuery(q1) = peer.next().await else {
        panic!("a trust query");
    };
    assert_eq!(q1.namespace, "lab");
    // Unanswered: it times out and is asked again.
    let F::TrustQuery(q2) = peer.next().await else {
        panic!("the trust query again");
    };
    assert_ne!(q1.query_id, q2.query_id);
    peer.send(F::TrustPage(mp::TrustPage {
        query_id: q2.query_id,
        bindings: vec![binding("lab", 9)],
        ..Default::default()
    }))
    .await;
    let path = w.store.clone();
    eventually("the lab binding", || {
        Store::open_read_only(&path)
            .unwrap()
            .binding("lab")
            .unwrap()
            .is_some()
    })
    .await;
    assert!(peer.quiet().await.is_none(), "no more queries once applied");
    adapter.stop().await.unwrap();
}

/// Trust changes for one namespace share one refresh: changes during it make
/// it run once more, not once each. A page refused for good is not asked
/// again; one refused for want of storage is.
#[tokio::test(flavor = "multi_thread")]
async fn trust_changes_coalesce_and_permanent_refusals_stop() {
    let mut w = World::new().await;
    let adapter = Running::start_with(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
        quick(Duration::from_secs(10)),
    );
    let mut peer = w.relay.accept().await;
    let changed = |namespace: &str| {
        F::TrustChanged(mp::TrustChanged {
            namespace: namespace.into(),
        })
    };
    let page = |q: mp::TrustQuery, error_code: &str| {
        F::TrustPage(mp::TrustPage {
            query_id: q.query_id,
            bindings: if error_code.is_empty() {
                vec![binding(&q.namespace, 9)]
            } else {
                vec![]
            },
            error_code: error_code.into(),
            ..Default::default()
        })
    };
    for _ in 0..3 {
        peer.send(changed("lab")).await;
    }
    let F::TrustQuery(q) = peer.next().await else {
        panic!("a trust query");
    };
    assert_eq!(q.namespace, "lab");
    peer.send(page(q, "")).await;
    // Changed again during the first: one more, for the latest state.
    let F::TrustQuery(q) = peer.next().await else {
        panic!("one more trust query");
    };
    assert_eq!(q.namespace, "lab");
    peer.send(page(q, "")).await;
    assert!(peer.quiet().await.is_none(), "not one per change");

    peer.send(changed("gone")).await;
    let F::TrustQuery(q) = peer.next().await else {
        panic!("a trust query");
    };
    peer.send(page(q, "storage_unavailable")).await;
    let F::TrustQuery(q) = peer.next().await else {
        panic!("asked again after a passing failure");
    };
    peer.send(page(q, "namespace_unknown")).await;
    assert!(peer.quiet().await.is_none(), "not asked again");
    adapter.stop().await.unwrap();
}

/// A refused route claim ends the stream; the adapter reconnects and claims
/// again, and work flows once a claim is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_route_claim_is_retried_on_a_new_stream() {
    let mut w = World::new().await;
    stored_sends(&w.store, 1);
    let adapter = Running::start(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
    );
    let mut first = w.relay.open().await;
    first.trust().await;
    let claim = first.route().await;
    first.route_result(claim, false).await;
    // Nothing is handed to a stream whose route was refused.
    while let Some(f) = first.next_any().await {
        assert!(matches!(f, F::Heartbeat(_)), "{f:?}");
    }
    let mut second = w.relay.accept().await;
    let F::Transfer(t) = second.next().await else {
        panic!("the send goes once the route is accepted");
    };
    assert_eq!(t.handoff_id, "h1");
    adapter.stop().await.unwrap();
}

/// Stopping ends the relay session: status no longer shows a registered
/// route, and the stream ends.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_ends_the_relay_session() {
    let mut w = World::new().await;
    let adapter = Running::start(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
    );
    let mut peer = w.relay.accept().await;
    let path = w.store.clone();
    eventually("the route is registered", || {
        Store::open_read_only(&path)
            .unwrap()
            .session()
            .unwrap()
            .is_some_and(|(_, _, ready, ended)| ready && !ended)
    })
    .await;
    adapter.stop().await.unwrap();
    let (_, _, _, ended) = w.store().session().unwrap().unwrap();
    assert!(ended, "the session is recorded as ended");
    while let Some(f) = peer.next_any().await {
        assert!(matches!(f, F::Heartbeat(_)), "{f:?}");
    }
}

/// A trust page's bad bindings (malformed, or an authority key of small
/// order) are skipped, not the page: its good bindings and revocations
/// still apply.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_binding_does_not_spoil_its_trust_page() {
    let mut w = World::new().await;
    let adapter = Running::start(
        settings(free_addr(), w.relay.addr, "unused", "bbs"),
        &w.store,
    );
    let mut peer = w.relay.open().await;
    let F::TrustQuery(q) = peer.next().await else {
        panic!("a trust query");
    };
    let mut identity = vec![0; 32];
    identity[0] = 1;
    let revoked = key(9).verifying_key().to_bytes().to_vec();
    let revocation = sign::sign(
        &key(HUB_AUTHORITY),
        mp::RecordKind::InstallationRevocation,
        mp::InstallationRevocation {
            namespace: "hub".into(),
            public_key: revoked.clone(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    peer.send(F::TrustPage(mp::TrustPage {
        query_id: q.query_id,
        bindings: vec![
            mp::NamespaceBinding {
                namespace: "weak".into(),
                authority_public_key: identity,
                revision: 1,
                disabled: false,
            },
            mp::NamespaceBinding {
                namespace: "short".into(),
                authority_public_key: vec![1; 31],
                revision: 1,
                disabled: false,
            },
            binding("hub", HUB_AUTHORITY),
            binding("bbs", BBS_AUTHORITY),
        ],
        revocations: vec![revocation],
        ..Default::default()
    }))
    .await;
    let claim = peer.route().await;
    peer.route_result(claim, true).await;
    adapter.stop().await.unwrap();
    let store = w.store();
    assert!(store.binding("hub").unwrap().is_some());
    assert!(store.binding("weak").unwrap().is_none());
    assert!(store.binding("short").unwrap().is_none());
    assert!(store.revoked("hub", &revoked).unwrap());
}
