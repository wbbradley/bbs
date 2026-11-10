//! An in-process fake adapter drives the integration stream end to end over
//! a real gRPC connection to a board in a temporary directory.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::MutexGuard;

use chrono::Utc;
use prost::Message as _;
use rusqlite::Connection;
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

use super::super::{Policy, TimerLimits};
use super::*;
use crate::service::{self, Caller};
use crate::{db, proto};
use bbs_core::hub_proto::msgproto_v1 as mp;
use pb::integration_client::IntegrationClient;

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);

struct Rig {
    _dir: tempfile::TempDir,
    state: Arc<State>,
    addr: SocketAddr,
    /// The integration `hub`'s credential.
    token: String,
    /// Agent `scout`, owned by human `will`: entity `will/scout`.
    agent: String,
}

fn human() -> Caller {
    Caller {
        human: Some("will".into()),
        ..Default::default()
    }
}

impl Rig {
    async fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bbs.db");
        // As the server opens it: WAL, foreign keys on.
        let mut conn = db::open(&path).unwrap();
        db::migrate(&mut conn).unwrap();
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let agent = service::register(
            &mut conn,
            &human(),
            proto::RegisterRequest {
                name: Some("scout".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let token = integration::admin_add(&mut conn, "hub").unwrap();
        let state = Arc::new(State {
            conn: std::sync::Mutex::new(conn),
            notify: watch::Sender::new(0),
            activity: watch::Sender::new(0),
            waiters: std::sync::Mutex::new(HashSet::new()),
            shutdown: watch::Sender::new(false),
            require_human_token: false,
            protocol: Policy::default(),
            auth_failures: std::sync::Mutex::new(HashMap::new()),
            timer_limits: TimerLimits::default(),
            clock: Arc::new(Utc::now),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tonic::transport::Server::builder()
            .max_concurrent_streams(Some(MAX_STREAMS_PER_CONNECTION))
            .add_service(service(state.clone(), Some(&path)).unwrap())
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                {
                    let state = state.clone();
                    async move { state.stopping().await }
                },
            );
        tokio::spawn(server);
        Rig {
            _dir: dir,
            state,
            addr,
            token,
            agent,
        }
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.state.conn.lock().unwrap()
    }

    /// Turn publication on or off, as `bbs admin` would, and wake streams
    /// rather than wait for their recheck.
    fn publish(&self, on: bool) {
        integration::admin_publish(&mut self.conn(), "hub", on).unwrap();
        self.state.changed();
    }

    /// Send as agent `scout`.
    fn send(&self, to: &str, body: &str) -> anyhow::Result<proto::SendResponse> {
        let out = service::send(
            &mut self.conn(),
            &Caller {
                token: Some(self.agent.clone()),
                ..Default::default()
            },
            proto::SendRequest {
                to: to.into(),
                body: body.into(),
                control: None,
            },
        );
        self.state.changed();
        out
    }

    fn outbox(&self, message: i64) -> (String, Option<String>) {
        db::integration::outbox_state(&self.conn(), message)
            .unwrap()
            .unwrap()
    }

    /// Wait for message `message`'s handoff to reach `state`: the stream
    /// records a send once the transport takes the frame.
    async fn settle(&self, message: i64, state: &str) {
        for _ in 0..100 {
            if self.outbox(message).0 == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(self.outbox(message).0, state);
    }

    /// Handoffs by state.
    fn states(&self) -> HashMap<String, i64> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT state, count(*) FROM integration_outbox GROUP BY state")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Wait until no handoff is left attempted (the stream records its
    /// claims in the background), then count them by state.
    async fn settled(&self) -> HashMap<String, i64> {
        for _ in 0..100 {
            let states = self.states();
            if !states.contains_key("attempted") {
                return states;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("handoffs left attempted: {:?}", self.states());
    }

    /// Queue `n` remote sends of `size`-byte bodies to `id:ada@hub`.
    fn backlog(&self, n: usize, size: usize) {
        for i in 0..n {
            let body = format!("{i:05}{}", "x".repeat(size - 5));
            self.send("id:ada@hub", &body).unwrap();
        }
    }

    /// Read as agent `scout`.
    fn read(&self) -> Vec<proto::Message> {
        service::read(
            &mut self.conn(),
            &Caller {
                token: Some(self.agent.clone()),
                ..Default::default()
            },
            proto::ReadRequest::default(),
        )
        .unwrap()
        .messages
    }

    async fn connect_with(&self, token: &str, hello: pb::AdapterHello) -> Result<Adapter, Status> {
        self.connect_on(
            Channel::from_shared(format!("http://{}", self.addr)).unwrap(),
            token,
            hello,
        )
        .await
    }

    async fn connect_on(
        &self,
        endpoint: Endpoint,
        token: &str,
        hello: pb::AdapterHello,
    ) -> Result<Adapter, Status> {
        let channel = endpoint.connect().await.unwrap();
        let mut client = IntegrationClient::new(channel)
            .max_decoding_message_size(8 << 20)
            .max_encoding_message_size(8 << 20);
        let (tx, rx) = mpsc::channel(16);
        tx.send(pb::AdapterFrame {
            payload: Some(In::Hello(hello)),
        })
        .await
        .unwrap();
        let mut req = Request::new(ReceiverStream::new(rx));
        if !token.is_empty() {
            req.metadata_mut()
                .insert(TOKEN_METADATA, token.parse().unwrap());
        }
        let rx = client.connect(req).await?.into_inner();
        Ok(Adapter { tx, rx })
    }

    /// Connect as `hub`, and take BBS's hello and first catalog.
    async fn connect(&self) -> (Adapter, pb::PublishedCatalog) {
        self.connect_as(&self.token, "hub").await
    }

    /// Connect as integration `id`, and take BBS's hello and first catalog.
    async fn connect_as(&self, token: &str, id: &str) -> (Adapter, pb::PublishedCatalog) {
        let adapter = self.connect_with(token, hello(id)).await.unwrap();
        Self::greeted(adapter, id).await
    }

    async fn greeted(mut adapter: Adapter, id: &str) -> (Adapter, pb::PublishedCatalog) {
        let Out::Hello(hello) = adapter.next().await.unwrap() else {
            panic!("expected hello");
        };
        assert_eq!(hello.integration_id, id);
        assert_eq!(hello.protocol_major, 1);
        assert_eq!(hello.namespace, "bbs");
        let Out::Published(catalog) = adapter.next().await.unwrap() else {
            panic!("expected the catalog");
        };
        (adapter, catalog)
    }
}

fn hello(id: &str) -> pb::AdapterHello {
    pb::AdapterHello {
        integration_id: id.into(),
        protocol_major: 1,
        build: "fake-adapter".into(),
    }
}

struct Adapter {
    tx: mpsc::Sender<pb::AdapterFrame>,
    rx: Streaming<pb::BbsFrame>,
}

impl Adapter {
    async fn next(&mut self) -> Result<Out, Status> {
        let frame = timeout(WAIT, self.rx.message())
            .await
            .expect("a frame within five seconds")?
            .expect("the stream is open");
        Ok(frame.payload.unwrap())
    }

    /// Nothing arrives for a while.
    async fn quiet(&mut self) {
        if let Ok(frame) = timeout(QUIET, self.rx.message()).await {
            panic!("unexpected frame: {frame:?}");
        }
    }

    async fn send(&self, payload: In) {
        self.tx
            .send(pb::AdapterFrame {
                payload: Some(payload),
            })
            .await
            .unwrap();
    }

    async fn offer(&mut self, handoff: &str, bytes: Vec<u8>) -> pb::HandoffResult {
        self.send(In::Incoming(pb::MessageOffer {
            handoff_id: handoff.into(),
            message: Some(mp::EncodedMessage {
                format_major: 1,
                message_bytes: bytes,
            }),
        }))
        .await;
        match self.next().await.unwrap() {
            Out::IncomingResult(result) => {
                assert_eq!(result.handoff_id, handoff);
                result
            }
            other => panic!("expected an incoming result, got {other:?}"),
        }
    }

    async fn outgoing(&mut self) -> (pb::MessageOffer, mp::Message) {
        match self.next().await.unwrap() {
            Out::Outgoing(offer) => {
                let bytes = &offer.message.as_ref().unwrap().message_bytes;
                let message = mp::Message::decode(bytes.as_slice()).unwrap();
                (offer, message)
            }
            other => panic!("expected an outgoing offer, got {other:?}"),
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

/// A message from `hub`'s `from` to this board's `to`.
fn message(id: &str, from: &str, to: &str, body: &str) -> mp::Message {
    let now = Utc::now().timestamp_micros();
    mp::Message {
        id: Some(mp::MessageRef {
            namespace: "hub".into(),
            message_id: id.into(),
        }),
        sender: address("hub", from),
        recipient: address("bbs", to),
        authored_at_unix_micros: now,
        expires_at_unix_micros: Some(now + 86_400_000_000),
        content_type: integration::CONTENT_TYPE.into(),
        body: body.as_bytes().to_vec(),
        kind: mp::MessageKind::Application as i32,
        ..Default::default()
    }
}

fn rejected(result: &pb::HandoffResult, reason: &str) {
    assert_eq!(
        result.disposition,
        pb::HandoffDisposition::Rejected as i32,
        "{result:?}"
    );
    assert_eq!(result.reason, reason, "{result:?}");
}

fn received(result: &pb::HandoffResult) {
    assert_eq!(
        result.disposition,
        pb::HandoffDisposition::Received as i32,
        "{result:?}"
    );
}

fn entity_ids(catalog: &pb::PublishedCatalog) -> Vec<String> {
    catalog
        .entities
        .iter()
        .map(|e| e.entity.as_ref().unwrap().entity_id.clone())
        .collect()
}

#[tokio::test]
async fn refuses_bad_credentials_and_hellos() {
    let mut rig = Rig::new().await;
    for token in ["", "not-a-credential", rig.agent.as_str()] {
        let err = rig.connect_with(token, hello("hub")).await.err().unwrap();
        assert_eq!(err.code(), tonic::Code::Unauthenticated, "{token}: {err:?}");
    }
    let err = rig
        .connect_with(&rig.token, hello("other"))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
    let mut future = hello("hub");
    future.protocol_major = 2;
    let err = rig.connect_with(&rig.token, future).await.err().unwrap();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    assert!(err.message().contains("unsupported_protocol"), "{err:?}");

    // Revoking ends an open stream at its next recheck, and refuses new ones.
    let (mut adapter, _) = rig.connect().await;
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    let err = timeout(Duration::from_secs(8), adapter.rx.message())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");
    let err = rig
        .connect_with(&rig.token, hello("hub"))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    // A rotated credential works again.
    let rotated = integration::admin_rotate(&mut rig.conn(), "hub").unwrap();
    rig.token = rotated;
    rig.connect().await;
}

#[tokio::test]
async fn publication_off_advertises_nothing_and_refuses_sends() {
    let rig = Rig::new().await;
    let (mut adapter, catalog) = rig.connect().await;
    assert!(catalog.entities.is_empty());
    assert!(catalog.revision >= 1);

    let err = rig.send("id:ada@hub", "hello").unwrap_err();
    assert!(err.to_string().contains("remote sends are off"), "{err:#}");
    // The refusal wrote nothing.
    assert_eq!(
        rig.count("SELECT count(*) FROM entities WHERE namespace = 'hub'"),
        0
    );
    assert_eq!(rig.count("SELECT count(*) FROM messages"), 0);
    let result = adapter
        .offer(
            "h1",
            message("m1", "bob", "will/scout", "hi").encode_to_vec(),
        )
        .await;
    rejected(&result, "recipient_unknown");
    adapter
        .send(In::LocalDirectoryQuery(mp::DirectoryQuery {
            query_id: "q1".into(),
            namespace: "bbs".into(),
            ..Default::default()
        }))
        .await;
    let Out::LocalDirectoryResult(result) = adapter.next().await.unwrap() else {
        panic!("expected a directory result");
    };
    assert_eq!(result.query_id, "q1");
    assert!(result.entities.is_empty() && result.error_code.is_empty());
    adapter.quiet().await;
}

#[tokio::test]
async fn publication_on_advertises_humans_and_agents_with_revisions() {
    let rig = Rig::new().await;
    let (mut adapter, off) = rig.connect().await;
    rig.publish(true);
    let Out::Published(on) = adapter.next().await.unwrap() else {
        panic!("expected a new catalog");
    };
    assert_eq!(on.revision, off.revision + 1);
    assert_eq!(entity_ids(&on), ["will", "will/scout"]);
    let scout = &on.entities[1];
    assert_eq!(scout.kind, "agent");
    assert_eq!(scout.display_name, "scout");
    let native = &scout.addresses[0];
    assert_eq!(native.address.as_ref().unwrap().address_id, "native");
    assert_eq!(native.content_types, [integration::CONTENT_TYPE]);
    assert!(native.is_default);
    // Groups are not published.
    service::group_register(
        &mut rig.conn(),
        &human(),
        proto::GroupRegisterRequest {
            group: "team".into(),
            ..Default::default()
        },
    )
    .unwrap();
    rig.state.changed();
    adapter.quiet().await;

    // A reconnect repeats the snapshot under its revision.
    let (mut again, repeat) = rig.connect().await;
    assert_eq!(repeat, on);

    // A new agent, then a retirement, each publish a newer snapshot.
    service::register(
        &mut rig.conn(),
        &human(),
        proto::RegisterRequest {
            name: Some("ranger".into()),
            ..Default::default()
        },
    )
    .unwrap();
    rig.state.changed();
    let Out::Published(added) = again.next().await.unwrap() else {
        panic!("expected a new catalog");
    };
    assert_eq!(added.revision, on.revision + 1);
    assert_eq!(entity_ids(&added), ["will", "will/ranger", "will/scout"]);
    service::retire_agent(
        &mut rig.conn(),
        &human(),
        proto::RetireRequest {
            agent: "ranger".into(),
        },
    )
    .unwrap();
    rig.state.changed();
    let Out::Published(retired) = again.next().await.unwrap() else {
        panic!("expected a new catalog");
    };
    assert_eq!(retired.revision, added.revision + 1);
    assert_eq!(entity_ids(&retired), ["will", "will/scout"]);

    // Off again: an empty, newer snapshot withdraws publication.
    rig.publish(false);
    let Out::Published(withdrawn) = again.next().await.unwrap() else {
        panic!("expected a new catalog");
    };
    assert_eq!(withdrawn.revision, retired.revision + 1);
    assert!(withdrawn.entities.is_empty());
}

#[tokio::test]
async fn outbound_sends_are_claimed_once_and_record_results() {
    let rig = Rig::new().await;
    rig.publish(true);
    // Queued while no adapter is connected.
    let first = rig.send("id:ada@hub", "first").unwrap();
    assert_eq!(first.to, "id:ada@hub");
    let first = first.id.unwrap();
    assert_eq!(rig.outbox(first).0, "pending");
    let second = rig.send("id:bob@hub:slack#r1", "second").unwrap();
    assert_eq!(second.to, "id:bob@hub:slack#r1");
    let second = second.id.unwrap();

    let (mut adapter, _) = rig.connect().await;
    let (offer1, m1) = adapter.outgoing().await;
    let (offer2, m2) = adapter.outgoing().await;
    assert_eq!(m1.sender, address("bbs", "will/scout"));
    assert_eq!(m1.recipient, address("hub", "ada"));
    assert_eq!(m1.body, b"first");
    assert_eq!(m1.content_type, integration::CONTENT_TYPE);
    assert!(m1.expires_at_unix_micros.unwrap() > m1.authored_at_unix_micros);
    let mut to = address("hub", "bob").unwrap();
    to.address_id = "slack".into();
    to.run_id = Some("r1".into());
    assert_eq!(m2.recipient, Some(to));
    assert_ne!(offer1.handoff_id, offer2.handoff_id);
    rig.settle(first, "sent").await;
    rig.settle(second, "sent").await;
    // The offered bytes are the ones BBS stored.
    let stored =
        db::integration::origin_bytes(&rig.conn(), "bbs", &m1.id.as_ref().unwrap().message_id)
            .unwrap()
            .unwrap()
            .unwrap();
    assert_eq!(stored, offer1.message.as_ref().unwrap().message_bytes);

    adapter
        .send(In::OutgoingResult(pb::HandoffResult {
            handoff_id: offer1.handoff_id.clone(),
            disposition: pb::HandoffDisposition::Received as i32,
            ..Default::default()
        }))
        .await;
    adapter
        .send(In::OutgoingResult(pb::HandoffResult {
            handoff_id: offer2.handoff_id.clone(),
            disposition: pb::HandoffDisposition::Rejected as i32,
            reason: "recipient_unknown".into(),
            detail: "no such run".into(),
        }))
        .await;
    // A later result never replaces the first; an unknown handoff is ignored.
    adapter
        .send(In::OutgoingResult(pb::HandoffResult {
            handoff_id: offer2.handoff_id.clone(),
            disposition: pb::HandoffDisposition::Received as i32,
            ..Default::default()
        }))
        .await;
    adapter
        .send(In::OutgoingResult(pb::HandoffResult {
            handoff_id: "nobody".into(),
            disposition: pb::HandoffDisposition::Received as i32,
            ..Default::default()
        }))
        .await;
    adapter.send(In::Ping(pb::Ping { nonce: 7 })).await;
    let Out::Pong(pong) = adapter.next().await.unwrap() else {
        panic!("expected pong");
    };
    assert_eq!(pong.nonce, 7);
    assert_eq!(rig.outbox(first), ("received".into(), None));
    assert_eq!(
        rig.outbox(second),
        (
            "rejected".into(),
            Some("recipient_unknown: no such run".into())
        )
    );

    // The rejection, not the receipt, tells the sender, in a generic form
    // that leaves out the adapter's detail.
    let mail = rig.read();
    assert_eq!(mail.len(), 1, "{mail:?}");
    assert_eq!(mail[0].from_name, "scout");
    assert_eq!(
        mail[0].body,
        format!(
            "Not delivered: your message to id:bob@hub:slack#r1 (sent {}) was not \
             delivered: the recipient is unknown.",
            rig.conn()
                .query_row(
                    "SELECT created_at FROM messages WHERE id = ?1",
                    [second],
                    |r| r.get::<_, String>(0)
                )
                .unwrap()
        )
    );
    let notice: (String, String, String) = rig
        .conn()
        .query_row(
            "SELECT kind, in_reply_to_namespace, in_reply_to_message_id FROM messages
             WHERE id = ?1",
            [mail[0].id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        notice,
        (
            "delivery_error".into(),
            "bbs".into(),
            m2.id.as_ref().unwrap().message_id.clone()
        )
    );

    // Nothing is offered twice: not on this stream, nor after a reconnect.
    adapter.quiet().await;
    let (mut again, _) = rig.connect().await;
    again.quiet().await;

    // History shows the remote recipient.
    let feed = service::feed(
        &mut rig.conn(),
        &human(),
        proto::FeedRequest {
            backlog: 10,
            ..Default::default()
        },
    )
    .unwrap();
    let names: Vec<_> = feed
        .messages
        .iter()
        .map(|m| (m.from_name.as_str(), m.to_name.as_deref()))
        .collect();
    assert_eq!(
        names,
        [
            ("scout", Some("id:ada@hub")),
            ("scout", Some("id:bob@hub:slack#r1")),
            ("scout", Some("scout"))
        ]
    );
}

#[tokio::test]
async fn a_claim_interrupted_before_its_send_is_never_replayed() {
    let rig = Rig::new().await;
    rig.publish(true);
    let lost = rig.send("id:ada@hub", "lost").unwrap().id.unwrap();
    // The server claims the handoff (committed as attempted), then stops
    // before putting it on the stream.
    let (outbox, _) = integration::claim(&mut rig.conn(), "hub").unwrap().unwrap();
    assert_eq!(rig.outbox(lost).0, "attempted");
    // Attempted work is not offered, even before the restart.
    let (mut adapter, _) = rig.connect().await;
    adapter.quiet().await;
    // The restart marks it unknown and tells the sender; still nothing is
    // offered.
    assert_eq!(
        integration::interrupt_attempted(&mut rig.conn()).unwrap(),
        1
    );
    assert_eq!(
        rig.outbox(lost),
        (
            "unknown".into(),
            Some("interrupted: the server stopped during the handoff".into())
        )
    );
    let (mut adapter, _) = rig.connect().await;
    adapter.quiet().await;
    // A late send completion cannot revive it.
    integration::handed_off(&rig.conn(), outbox).unwrap();
    assert_eq!(rig.outbox(lost).0, "unknown");
    // New work still flows.
    let next = rig.send("id:ada@hub", "next").unwrap().id.unwrap();
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"next");
    rig.settle(next, "sent").await;
    // The sender heard once that delivery is uncertain, in words unlike a
    // failure's; nothing was resent.
    let mail = rig.read();
    assert_eq!(mail.len(), 1, "{mail:?}");
    let created: String = rig
        .conn()
        .query_row(
            "SELECT created_at FROM messages WHERE id = ?1",
            [lost],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        mail[0].body,
        format!(
            "Delivery uncertain: your message to id:ada@hub (sent {created}) may not have \
             arrived. BBS will not send it again."
        )
    );
    assert_eq!(rig.outbox(lost).0, "unknown");
    adapter.quiet().await;
}

#[tokio::test]
async fn imports_reach_the_inbox_with_the_foreign_sender_shown() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let mut m = message("m1", "bob", "will/scout", "hello from hub");
    m.sender.as_mut().unwrap().run_id = Some("r9".into());
    m.in_reply_to = Some(mp::MessageRef {
        namespace: "bbs".into(),
        message_id: "abc".into(),
    });
    received(&adapter.offer("h1", m.encode_to_vec()).await);

    let read = service::read(
        &mut rig.conn(),
        &Caller {
            token: Some(rig.agent.clone()),
            ..Default::default()
        },
        proto::ReadRequest::default(),
    )
    .unwrap();
    assert_eq!(read.messages.len(), 1);
    let got = &read.messages[0];
    assert_eq!(got.from_name, "id:bob@hub#r9");
    assert_eq!(got.from_kind(), proto::Kind::Remote);
    assert_eq!(got.from_owner, None);
    assert_eq!(got.body, "hello from hub");
    assert_eq!(got.to_name.as_deref(), Some("scout"));
    let feed = service::feed(
        &mut rig.conn(),
        &human(),
        proto::FeedRequest {
            backlog: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(feed.messages.len(), 1);
    assert_eq!(feed.messages[0].from_name, "id:bob@hub#r9");
    assert_eq!(feed.messages[0].to_name.as_deref(), Some("scout"));
    let reply: (String, String) = rig
        .conn()
        .query_row(
            "SELECT in_reply_to_namespace, in_reply_to_message_id FROM messages
             WHERE origin_message_id = 'm1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(reply, ("bbs".into(), "abc".into()));
    // Filtering by a remote entity is refused plainly, not as an unknown name.
    let err = service::search(
        &mut rig.conn(),
        &human(),
        proto::SearchRequest {
            query: "hello".into(),
            from: Some("id:bob@hub#r9".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("not supported yet"), "{err:#}");
    let err = service::feed(
        &mut rig.conn(),
        &human(),
        proto::FeedRequest {
            backlog: 10,
            with: Some("id:bob@hub".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("not supported yet"), "{err:#}");
    // The adapter's observation names the sender a human; the history still
    // says remote: what the adapter claims never makes it one of ours.
    adapter
        .send(In::Observed(pb::ObservedEntities {
            entities: vec![mp::EntityDescriptor {
                entity: Some(mp::EntityRef {
                    namespace: "hub".into(),
                    entity_id: "bob".into(),
                }),
                kind: "human".into(),
                display_name: "Bob".into(),
                ..Default::default()
            }],
            observed_at_unix_micros: 1,
        }))
        .await;
    adapter.send(In::Ping(pb::Ping { nonce: 1 })).await;
    adapter.next().await.unwrap();
    let feed = service::feed(
        &mut rig.conn(),
        &human(),
        proto::FeedRequest {
            backlog: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(feed.messages[0].from_kind(), proto::Kind::Remote);
    let kind: String = rig
        .conn()
        .query_row(
            "SELECT kind FROM entities WHERE namespace = 'hub' AND entity_id = 'bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "human");

    // A group copy: from the group, keeping what was sent and by whom.
    let mut copy = message("c1", "team", "will", "to the team");
    copy.group_copy = Some(mp::GroupCopy {
        original: Some(mp::MessageRef {
            namespace: "hub".into(),
            message_id: "orig".into(),
        }),
        original_sender: address("hub", "carol"),
    });
    received(&adapter.offer("h2", copy.encode_to_vec()).await);
    let stored: (String, String, String, String) = rig
        .conn()
        .query_row(
            "SELECT m.group_copy_namespace, m.group_copy_message_id, e.entity_id, se.kind
             FROM messages m
             JOIN entity_addresses a ON a.id = m.group_copy_sender_address_id
             JOIN entities e ON e.id = a.entity_id
             JOIN entity_addresses sa ON sa.id = m.sender_address_id
             JOIN entities se ON se.id = sa.entity_id
             WHERE m.origin_message_id = 'c1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        ("hub".into(), "orig".into(), "carol".into(), "group".into())
    );
    // A copy carrying in_reply_to is malformed.
    let mut bad = copy.clone();
    bad.id.as_mut().unwrap().message_id = "c2".into();
    bad.in_reply_to = Some(mp::MessageRef {
        namespace: "hub".into(),
        message_id: "x".into(),
    });
    rejected(
        &adapter.offer("h3", bad.encode_to_vec()).await,
        "invalid_message",
    );
    // An original sender outside the original's namespace, or one of ours,
    // is malformed.
    let mut elsewhere = copy.clone();
    elsewhere.id.as_mut().unwrap().message_id = "c3".into();
    elsewhere.group_copy.as_mut().unwrap().original_sender = address("acme", "carol");
    rejected(
        &adapter.offer("h4", elsewhere.encode_to_vec()).await,
        "invalid_message",
    );
    let mut native = copy.clone();
    native.id.as_mut().unwrap().message_id = "c4".into();
    native.group_copy = Some(mp::GroupCopy {
        original: Some(mp::MessageRef {
            namespace: "bbs".into(),
            message_id: "orig".into(),
        }),
        original_sender: address("bbs", "will/scout"),
    });
    rejected(
        &adapter.offer("h5", native.encode_to_vec()).await,
        "invalid_message",
    );
}

#[tokio::test]
async fn duplicates_match_and_conflicts_are_refused() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let bytes = message("m1", "bob", "will/scout", "once").encode_to_vec();
    received(&adapter.offer("h1", bytes.clone()).await);
    let again = adapter.offer("h2", bytes.clone()).await;
    assert_eq!(
        again.disposition,
        pb::HandoffDisposition::AlreadyKnown as i32
    );
    let conflict = adapter
        .offer(
            "h3",
            message("m1", "bob", "will/scout", "changed").encode_to_vec(),
        )
        .await;
    rejected(&conflict, "message_conflict");
    let deliveries: i64 = rig
        .conn()
        .query_row("SELECT count(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(deliveries, 1);
    // A duplicate is answered even after its recipient retired.
    service::retire_agent(
        &mut rig.conn(),
        &human(),
        proto::RetireRequest {
            agent: "scout".into(),
        },
    )
    .unwrap();
    let late = adapter.offer("h4", bytes).await;
    assert_eq!(
        late.disposition,
        pb::HandoffDisposition::AlreadyKnown as i32,
        "{late:?}"
    );
}

#[tokio::test]
async fn retired_unknown_and_group_targets_are_refused_alike() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    service::group_register(
        &mut rig.conn(),
        &human(),
        proto::GroupRegisterRequest {
            group: "team".into(),
            ..Default::default()
        },
    )
    .unwrap();
    service::retire_agent(
        &mut rig.conn(),
        &human(),
        proto::RetireRequest {
            agent: "scout".into(),
        },
    )
    .unwrap();
    let to_retired = adapter
        .offer(
            "h1",
            message("m1", "bob", "will/scout", "hi").encode_to_vec(),
        )
        .await;
    // Retired, absent, a registered group, another namespace: one answer,
    // which tells none of them apart.
    let mut results = vec![to_retired];
    for (n, to) in ["will/nobody", "team"].into_iter().enumerate() {
        let id = format!("u{n}");
        results.push(
            adapter
                .offer(&id, message(&id, "bob", to, "hi").encode_to_vec())
                .await,
        );
    }
    let mut elsewhere = message("m2", "bob", "will", "hi");
    elsewhere.recipient = address("acme", "will");
    results.push(adapter.offer("h2", elsewhere.encode_to_vec()).await);
    for result in &results {
        rejected(result, "recipient_unknown");
        assert_eq!(result.detail, "no such recipient");
    }
    // Nothing was stored.
    let messages: i64 = rig
        .conn()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let before: i64 = 0;
    assert_eq!(messages, before);
}

#[tokio::test]
async fn malformed_and_oversize_offers_are_refused() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let ok = || message("m1", "bob", "will", "hi");
    let mut cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("garbage", vec![0xff, 0xff, 0xff], "invalid_message"),
        (
            "too-big",
            vec![b'x'; integration::MAX_MESSAGE_BYTES + 1],
            "frame_too_large",
        ),
    ];
    let mut long_body = ok();
    long_body.body = vec![b'x'; integration::MAX_BODY_BYTES + 1];
    cases.push(("long-body", long_body.encode_to_vec(), "frame_too_large"));
    let mut no_deadline = ok();
    no_deadline.expires_at_unix_micros = None;
    cases.push((
        "no-deadline",
        no_deadline.encode_to_vec(),
        "invalid_message",
    ));
    let mut deep = ok();
    deep.sender = address("hub", "a/b/c");
    cases.push(("three-levels", deep.encode_to_vec(), "invalid_message"));
    let mut long_id = ok();
    long_id.sender = address("hub", &"a".repeat(129));
    cases.push(("long-id", long_id.encode_to_vec(), "invalid_message"));
    let mut ours = ok();
    ours.id.as_mut().unwrap().namespace = "bbs".into();
    ours.sender = address("bbs", "will");
    cases.push((
        "own-namespace",
        ours.encode_to_vec(),
        "route_not_authorized",
    ));
    let mut spoof = ok();
    spoof.sender = address("bbs", "will");
    cases.push(("spoofed-sender", spoof.encode_to_vec(), "invalid_message"));
    let mut html = ok();
    html.content_type = "text/html".into();
    cases.push(("html", html.encode_to_vec(), "unsupported_content"));
    let mut expired = ok();
    expired.expires_at_unix_micros = Some(1);
    cases.push(("expired", expired.encode_to_vec(), "expired"));
    let mut empty = ok();
    empty.body = b" \n\t".to_vec();
    cases.push(("blank-body", empty.encode_to_vec(), "invalid_message"));
    let mut error = ok();
    error.kind = mp::MessageKind::DeliveryError as i32;
    cases.push((
        "delivery-error",
        error.encode_to_vec(),
        "unsupported_content",
    ));
    for (handoff, bytes, reason) in cases {
        let result = adapter.offer(handoff, bytes).await;
        rejected(&result, reason);
    }
    rejected(
        &adapter.offer("bad handoff", ok().encode_to_vec()).await,
        "invalid_message",
    );

    // A frame with no payload ends the stream.
    adapter
        .tx
        .send(pb::AdapterFrame { payload: None })
        .await
        .unwrap();
    let err = adapter.next().await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    // So does a second hello, and a result with no disposition.
    let (mut adapter, _) = rig.connect().await;
    adapter.send(In::Hello(hello("hub"))).await;
    assert_eq!(
        adapter.next().await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    let (mut adapter, _) = rig.connect().await;
    adapter
        .send(In::OutgoingResult(pb::HandoffResult {
            handoff_id: "h".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(
        adapter.next().await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    // A frame over 4 MiB ends the stream before it is decoded.
    let (mut adapter, _) = rig.connect().await;
    adapter
        .send(In::Incoming(pb::MessageOffer {
            handoff_id: "huge".into(),
            message: Some(mp::EncodedMessage {
                format_major: 1,
                message_bytes: vec![0; integration::MAX_FRAME_BYTES + 1],
            }),
        }))
        .await;
    let err = adapter.next().await.unwrap_err();
    assert!(
        matches!(
            err.code(),
            tonic::Code::OutOfRange | tonic::Code::ResourceExhausted
        ),
        "{err:?}"
    );
    let messages: i64 = rig
        .conn()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(messages, 0);
}

#[tokio::test]
async fn the_directory_answers_for_published_entities_in_pages() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let query = |page_token: Vec<u8>| mp::DirectoryQuery {
        query_id: "q".into(),
        namespace: "bbs".into(),
        page_size: 1,
        page_token,
        ..Default::default()
    };
    let first = ask(&mut adapter, query(Vec::new())).await;
    assert_eq!(entity_ids_of(&first), ["will"]);
    assert_eq!(first.next_page_token, b"will");
    let second = ask(&mut adapter, query(first.next_page_token)).await;
    assert_eq!(entity_ids_of(&second), ["will/scout"]);
    assert_eq!(
        second.entities[0].addresses[0].allowed_operations,
        [mp::Operation::Send as i32]
    );
    assert!(second.next_page_token.is_empty());
    let exact = ask(
        &mut adapter,
        mp::DirectoryQuery {
            query_id: "q".into(),
            namespace: "bbs".into(),
            entity_id: "will/scout".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(entity_ids_of(&exact), ["will/scout"]);
    let elsewhere = ask(
        &mut adapter,
        mp::DirectoryQuery {
            query_id: "q".into(),
            namespace: "acme".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(elsewhere.error_code, "namespace_unknown");
    let bad = ask(&mut adapter, query(b"../x".to_vec())).await;
    assert_eq!(bad.error_code, "invalid_cursor");
}

async fn ask(adapter: &mut Adapter, q: mp::DirectoryQuery) -> mp::DirectoryResult {
    adapter.send(In::LocalDirectoryQuery(q)).await;
    match adapter.next().await.unwrap() {
        Out::LocalDirectoryResult(result) => result,
        other => panic!("expected a directory result, got {other:?}"),
    }
}

fn entity_ids_of(result: &mp::DirectoryResult) -> Vec<String> {
    result
        .entities
        .iter()
        .map(|e| e.entity.as_ref().unwrap().entity_id.clone())
        .collect()
}

/// Read the adapter's side of the stream until it ends, a frame every
/// `pause`; returns the outgoing offers received and the ending status.
async fn drain(adapter: &mut Adapter, pause: Duration) -> (i64, Status) {
    let mut offers = 0;
    loop {
        tokio::time::sleep(pause).await;
        match timeout(Duration::from_secs(10), adapter.rx.message())
            .await
            .expect("a frame within ten seconds")
        {
            Ok(Some(pb::BbsFrame {
                payload: Some(Out::Outgoing(_)),
            })) => offers += 1,
            Ok(Some(frame)) => panic!("unexpected frame: {frame:?}"),
            Ok(None) => panic!("the stream ended without a status"),
            Err(status) => return (offers, status),
        }
    }
}

#[tokio::test]
async fn a_revoke_mid_backlog_counts_as_sent_only_what_reached_the_adapter() {
    let rig = Rig::new().await;
    rig.publish(true);
    rig.backlog(400, 1024);
    let (mut adapter, _) = rig.connect().await;
    for _ in 0..50 {
        adapter.outgoing().await;
    }
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    // A slow reader: the stream ends at the recheck, well before the backlog
    // is through.
    let (rest, status) = drain(&mut adapter, Duration::from_millis(20)).await;
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    let received = 50 + rest;
    assert!(received < 400, "{received}");
    let states = rig.settled().await;
    assert_eq!(states.get("sent").copied(), Some(received), "{states:?}");
    assert_eq!(states.values().sum::<i64>(), 400, "{states:?}");
    // Each unknown handoff, and only those, tells its sender delivery is
    // uncertain; none is offered again.
    let unknown = states.get("unknown").copied().unwrap_or(0);
    let mail = rig.read();
    assert_eq!(mail.len() as i64, unknown, "{states:?}");
    assert!(mail.iter().all(|m| {
        m.body
            .starts_with("Delivery uncertain: your message to id:ada@hub")
    }));
}

#[tokio::test]
async fn a_revoke_ends_a_stream_whose_adapter_stopped_reading() {
    let rig = Rig::new().await;
    rig.publish(true);
    rig.backlog(100, 4096);
    let endpoint = Channel::from_shared(format!("http://{}", rig.addr))
        .unwrap()
        .initial_stream_window_size(65_535)
        .initial_connection_window_size(65_535);
    let adapter = rig
        .connect_on(endpoint, &rig.token, hello("hub"))
        .await
        .unwrap();
    let (mut adapter, _) = Rig::greeted(adapter, "hub").await;
    // The adapter reads nothing more: the transport's windows and the
    // stream's queue fill, and the session waits for room.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rig.states().get("pending").copied().unwrap_or(0) > 0);
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    // The session ends anyway: what it had queued becomes unknown.
    let mut states = rig.states();
    for _ in 0..500 {
        if states.contains_key("unknown") && !states.contains_key("attempted") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        states = rig.states();
    }
    assert!(states.contains_key("unknown"), "{states:?}");
    assert!(!states.contains_key("attempted"), "{states:?}");
    // Once the adapter reads again, it gets exactly what was counted sent,
    // then the status.
    let (received, status) = drain(&mut adapter, Duration::ZERO).await;
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    let states = rig.settled().await;
    assert_eq!(
        states.get("sent").copied().unwrap_or(0),
        received,
        "{states:?}"
    );
    assert!(received < 100);
    // The handoffs it had queued are uncertain, and their senders are told.
    let mail = rig.read();
    assert_eq!(mail.len() as i64, states["unknown"], "{states:?}");
    assert!(
        mail.iter()
            .all(|m| m.body.starts_with("Delivery uncertain: "))
    );
}

#[tokio::test]
async fn revoking_disables_remote_sends_until_a_new_credential() {
    let rig = Rig::new().await;
    rig.publish(true);
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    assert_eq!(
        rig.count("SELECT enabled FROM bbs_integrations WHERE id = 'hub'"),
        0
    );
    let err = rig.send("id:ada@hub", "hi").unwrap_err();
    assert!(
        err.to_string().contains("no integration is enabled"),
        "{err:#}"
    );
    assert_eq!(rig.count("SELECT count(*) FROM messages"), 0);
    // Once bound, the namespace's integration is named instead.
    let token = integration::admin_rotate(&mut rig.conn(), "hub").unwrap();
    let (mut adapter, _) = rig.connect_as(&token, "hub").await;
    rig.send("id:ada@hub", "bound").unwrap();
    adapter.outgoing().await;
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    let err = rig.send("id:ada@hub", "hi").unwrap_err();
    assert!(
        err.to_string().contains("is disabled or has no credential"),
        "{err:#}"
    );
}

#[tokio::test]
async fn sends_to_our_namespace_or_a_retired_entity_are_refused_before_any_write() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let err = rig.send("id:will@bbs", "hi").unwrap_err();
    assert!(
        err.to_string().contains("this installation's namespace"),
        "{err:#}"
    );
    adapter
        .send(In::Observed(pb::ObservedEntities {
            entities: vec![mp::EntityDescriptor {
                entity: Some(mp::EntityRef {
                    namespace: "hub".into(),
                    entity_id: "ada".into(),
                }),
                kind: "human".into(),
                display_name: "Ada".into(),
                lifecycle: mp::Lifecycle::Retired as i32,
                ..Default::default()
            }],
            observed_at_unix_micros: 1,
        }))
        .await;
    adapter.send(In::Ping(pb::Ping { nonce: 1 })).await;
    adapter.next().await.unwrap();
    let err = rig.send("id:ada@hub:slack", "hi").unwrap_err();
    assert!(err.to_string().contains("is retired"), "{err:#}");
    assert_eq!(rig.count("SELECT count(*) FROM messages"), 0);
    assert_eq!(rig.count("SELECT count(*) FROM entity_addresses a JOIN entities e ON e.id = a.entity_id WHERE e.namespace = 'hub'"), 0);
    adapter.quiet().await;
}

#[tokio::test]
async fn focus_mode_holds_remote_sends_and_imports() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    service::set_focus(
        &mut rig.conn(),
        &human(),
        proto::SetFocusRequest {
            agent: "scout".into(),
            focused: true,
        },
    )
    .unwrap();
    assert!(rig.send("id:ada@hub", "hi").is_err());
    assert_eq!(rig.count("SELECT count(*) FROM integration_outbox"), 0);
    assert_eq!(
        rig.count("SELECT count(*) FROM entities WHERE namespace = 'hub'"),
        0
    );
    let result = adapter
        .offer(
            "h1",
            message("m1", "bob", "will/scout", "hi").encode_to_vec(),
        )
        .await;
    rejected(&result, "sender_not_permitted");
    assert_eq!(result.detail, "the recipient is not accepting messages");
    adapter.quiet().await;
}

#[tokio::test]
async fn work_queued_while_publication_is_off_waits_for_it() {
    let rig = Rig::new().await;
    rig.publish(true);
    let held = rig.send("id:ada@hub", "held").unwrap().id.unwrap();
    rig.publish(false);
    let (mut adapter, catalog) = rig.connect().await;
    assert!(catalog.entities.is_empty());
    adapter.quiet().await;
    assert_eq!(rig.outbox(held).0, "pending");
    rig.publish(true);
    let Out::Published(on) = adapter.next().await.unwrap() else {
        panic!("expected the catalog");
    };
    assert!(!on.entities.is_empty());
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"held");
    rig.settle(held, "sent").await;
}

#[tokio::test]
async fn a_newer_stream_supersedes_an_older_one() {
    let rig = Rig::new().await;
    let (mut old, _) = rig.connect().await;
    let (mut new, _) = rig.connect().await;
    assert_eq!(old.next().await.unwrap_err().code(), tonic::Code::Aborted);
    new.send(In::Ping(pb::Ping { nonce: 3 })).await;
    let Out::Pong(pong) = new.next().await.unwrap() else {
        panic!("expected pong");
    };
    assert_eq!(pong.nonce, 3);
}

/// The notice `message` got, if any.
fn notice_body(rig: &Rig, message: i64) -> Option<String> {
    let created: String = rig
        .conn()
        .query_row(
            "SELECT created_at FROM messages WHERE id = ?1",
            [message],
            |r| r.get(0),
        )
        .unwrap();
    rig.read().into_iter().find_map(|m| {
        m.body
            .contains(&format!("(sent {created})"))
            .then_some(m.body)
    })
}

fn expire_now(rig: &Rig, message: i64) {
    rig.conn()
        .execute(
            "UPDATE messages SET expires_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
            [message],
        )
        .unwrap();
}

#[tokio::test]
async fn pending_work_expires_after_seven_days_and_tells_its_sender() {
    let rig = Rig::new().await;
    rig.publish(true);
    // The periodic sweep, by the clock.
    let late = rig.send("id:ada@hub", "late").unwrap().id.unwrap();
    let now = Utc::now();
    assert_eq!(
        integration::expire(&mut rig.conn(), now + chrono::Duration::days(6)).unwrap(),
        0
    );
    assert_eq!(rig.outbox(late).0, "pending");
    assert_eq!(
        integration::expire(&mut rig.conn(), now + chrono::Duration::days(8)).unwrap(),
        1
    );
    let (state, detail) = rig.outbox(late);
    assert_eq!(state, "rejected");
    assert!(detail.unwrap().starts_with("expired: "));
    let body = notice_body(&rig, late).unwrap();
    assert!(
        body.ends_with("was not delivered: it expired before it could be delivered."),
        "{body}"
    );

    // The server's sweep runs at once on start.
    let swept = rig.send("id:ada@hub", "swept").unwrap().id.unwrap();
    expire_now(&rig, swept);
    tokio::spawn(sweep(rig.state.clone()));
    rig.settle(swept, "rejected").await;
    assert!(notice_body(&rig, swept).is_some());

    // Work that expires before the sweep reaches it is refused at the claim.
    let claimed = rig.send("id:ada@hub", "claimed").unwrap().id.unwrap();
    expire_now(&rig, claimed);
    let fresh = rig.send("id:ada@hub", "fresh").unwrap().id.unwrap();
    let (mut adapter, _) = rig.connect().await;
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"fresh");
    adapter.quiet().await;
    assert_eq!(rig.outbox(claimed).0, "rejected");
    assert!(notice_body(&rig, claimed).is_some());
    rig.settle(fresh, "sent").await;
    assert!(notice_body(&rig, fresh).is_none());
}

#[tokio::test]
async fn each_namespace_belongs_to_the_first_integration_to_use_it() {
    let rig = Rig::new().await;
    rig.publish(true);
    let other = integration::admin_add(&mut rig.conn(), "other").unwrap();
    integration::admin_publish(&mut rig.conn(), "other", true).unwrap();
    // Two integrations, and neither reaches `hub` yet: BBS cannot choose.
    let err = rig.send("id:ada@hub", "hi").unwrap_err();
    assert!(err.to_string().contains("several integrations"), "{err:#}");

    let (mut hub, _) = rig.connect().await;
    let (mut acme, _) = rig.connect_as(&other, "other").await;
    let first = message("m1", "bob", "will/scout", "from hub");
    received(&hub.offer("h1", first.encode_to_vec()).await);
    // `hub` is now bound to integration `hub`: another integration cannot
    // speak for it, even repeating the same message.
    rejected(
        &acme.offer("a1", first.encode_to_vec()).await,
        "route_not_authorized",
    );
    let mut from_acme = message("m1", "eve", "will/scout", "from acme");
    from_acme.id.as_mut().unwrap().namespace = "acme".into();
    from_acme.sender = address("acme", "eve");
    received(&acme.offer("a2", from_acme.encode_to_vec()).await);
    rejected(
        &hub.offer("h2", from_acme.encode_to_vec()).await,
        "route_not_authorized",
    );

    // Sends follow the bindings.
    rig.send("id:ada@hub", "to hub").unwrap();
    rig.send("id:eve@acme", "to acme").unwrap();
    let (_, m) = hub.outgoing().await;
    assert_eq!(m.body, b"to hub");
    let (_, m) = acme.outgoing().await;
    assert_eq!(m.body, b"to acme");
    hub.quiet().await;
    acme.quiet().await;
    let bound: Vec<_> = integration::admin_list(&mut rig.conn())
        .unwrap()
        .into_iter()
        .map(|row| (row.id, row.namespaces))
        .collect();
    assert_eq!(
        bound,
        [
            ("hub".to_string(), vec!["hub".to_string()]),
            ("other".to_string(), vec!["acme".to_string()])
        ]
    );

    // Integration `other` replaces `hub`: unbinding moves `hub` to the next
    // integration that uses it, addresses already known included.
    assert_eq!(
        integration::admin_unbind(&mut rig.conn(), "hub").unwrap(),
        Some("hub".into())
    );
    assert_eq!(
        integration::admin_unbind(&mut rig.conn(), "hub").unwrap(),
        None
    );
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    rig.send("id:ada@hub", "via other").unwrap();
    let (_, m) = acme.outgoing().await;
    assert_eq!(m.body, b"via other");
    let mut again = message("m2", "bob", "will/scout", "now via other");
    again.sender = address("hub", "bob");
    received(&acme.offer("a3", again.encode_to_vec()).await);
    assert_eq!(
        rig.count(
            "SELECT count(*) FROM address_delivery_bindings b
             JOIN entity_addresses a ON a.id = b.address_id
             JOIN entities e ON e.id = a.entity_id
             WHERE e.namespace = 'hub' AND b.integration_id != 'other'"
        ),
        0
    );
    let namespaces: Vec<_> = integration::admin_list(&mut rig.conn())
        .unwrap()
        .into_iter()
        .map(|row| row.namespaces)
        .collect();
    assert_eq!(
        namespaces,
        [vec![], vec!["acme".to_string(), "hub".to_string()]]
    );
}

#[tokio::test]
async fn an_oversize_catalog_is_not_sent_and_does_not_hold_up_the_outbox() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, before) = rig.connect().await;
    // Two entities of about 3 MB each: over one 4 MiB frame.
    rig.conn()
        .execute(
            "UPDATE entities SET display_name = replace(hex(zeroblob(1500000)), '0', 'x')
             WHERE namespace = 'bbs'",
            [],
        )
        .unwrap();
    let sent = rig.send("id:ada@hub", "still flows").unwrap().id.unwrap();
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"still flows");
    adapter.quiet().await;
    rig.settle(sent, "sent").await;
    // No revision was spent on it.
    let revision: i64 =
        rig.count("SELECT publication_revision FROM bbs_integrations WHERE id = 'hub'");
    assert_eq!(revision as u64, before.revision);
}
