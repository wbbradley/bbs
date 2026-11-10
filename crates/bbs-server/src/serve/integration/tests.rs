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
            integrations: Default::default(),
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
    let (outbox, _) = integration::claim(&mut rig.conn(), "hub")
        .unwrap()
        .handoff
        .unwrap();
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

/// A top-level ID answers as its own alias, as Hub's entities do by default,
/// so `hub send will@bbs` resolves; a second-level ID never does.
#[tokio::test]
async fn a_top_level_id_is_its_own_alias() {
    let rig = Rig::new().await;
    service::ensure_human(
        &mut rig.conn(),
        proto::EnsureHumanRequest { name: "Ada".into() },
    )
    .unwrap();
    let (mut adapter, _) = rig.connect().await;
    let by_alias = |alias: &str| mp::DirectoryQuery {
        query_id: "q".into(),
        namespace: "bbs".into(),
        alias: alias.into(),
        ..Default::default()
    };
    // Nothing answers while publication is off.
    let off = ask(&mut adapter, by_alias("will")).await;
    assert!(off.entities.is_empty() && off.error_code.is_empty());

    rig.publish(true);
    let Out::Published(catalog) = adapter.next().await.unwrap() else {
        panic!("expected a new catalog");
    };
    // The catalog lists the same aliases.
    let aliases: Vec<_> = catalog.entities.iter().map(|e| e.aliases.clone()).collect();
    assert_eq!(aliases, [vec!["ada"], vec!["will"], vec![]]);
    let will = ask(&mut adapter, by_alias("will")).await;
    assert_eq!(entity_ids_of(&will), ["will"]);
    assert_eq!(will.entities[0].aliases, ["will"]);
    assert!(will.entities[0].addresses[0].is_default);
    // Aliases are normalized lowercase; the ID keeps its case.
    assert_eq!(
        entity_ids_of(&ask(&mut adapter, by_alias("WILL")).await),
        ["will"]
    );
    let ada = ask(&mut adapter, by_alias("ada")).await;
    assert_eq!(entity_ids_of(&ada), ["Ada"]);
    assert_eq!(ada.entities[0].aliases, ["ada"]);
    // An agent's last segment is no alias, and its descriptor lists none.
    let scout = ask(&mut adapter, by_alias("scout")).await;
    assert!(scout.entities.is_empty() && scout.error_code.is_empty());
    let exact = ask(
        &mut adapter,
        mp::DirectoryQuery {
            entity_id: "will/scout".into(),
            ..by_alias("")
        },
    )
    .await;
    assert!(exact.entities[0].aliases.is_empty());
    // A path is not an alias at all, nor is anything outside Hub's form.
    for bad in ["will/scout", "_will", "w!ll", &"a".repeat(65)] {
        let r = ask(&mut adapter, by_alias(bad)).await;
        assert_eq!(r.error_code, "invalid_argument", "{bad}");
        assert!(r.entities.is_empty());
    }
    // An unknown alias is an empty answer, not an error.
    let none = ask(&mut adapter, by_alias("nobody")).await;
    assert!(none.entities.is_empty() && none.error_code.is_empty());
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
/// `pause`; returns the outgoing offers' bodies and the ending status.
async fn drain(adapter: &mut Adapter, pause: Duration) -> (Vec<Vec<u8>>, Status) {
    let mut bodies = Vec::new();
    loop {
        tokio::time::sleep(pause).await;
        match timeout(Duration::from_secs(10), adapter.rx.message())
            .await
            .expect("a frame within ten seconds")
        {
            Ok(Some(pb::BbsFrame {
                payload: Some(Out::Outgoing(offer)),
            })) => bodies.push(body_of(&offer)),
            Ok(Some(frame)) => panic!("unexpected frame: {frame:?}"),
            Ok(None) => panic!("the stream ended without a status"),
            Err(status) => return (bodies, status),
        }
    }
}

fn body_of(offer: &pb::MessageOffer) -> Vec<u8> {
    let bytes = &offer.message.as_ref().unwrap().message_bytes;
    mp::Message::decode(bytes.as_slice()).unwrap().body
}

/// After a revoke cut a backlog short: the adapter got exactly what was
/// counted sent, the rest is pending with no notice, and a new credential's
/// stream offers the rest, so every message arrives exactly once.
async fn finish_backlog(rig: &mut Rig, total: usize, mut got: Vec<Vec<u8>>) {
    let states = rig.settled().await;
    assert_eq!(
        states.get("sent").copied().unwrap_or(0),
        got.len() as i64,
        "{states:?}"
    );
    assert_eq!(
        states.get("pending").copied().unwrap_or(0),
        (total - got.len()) as i64,
        "{states:?}"
    );
    assert!(rig.read().is_empty());
    let token = integration::admin_rotate(&mut rig.conn(), "hub").unwrap();
    rig.token = token;
    let (mut adapter, _) = rig.connect().await;
    while got.len() < total {
        let (offer, _) = adapter.outgoing().await;
        got.push(body_of(&offer));
    }
    adapter.quiet().await;
    let unique: HashSet<_> = got.iter().collect();
    assert_eq!(unique.len(), total);
}

#[tokio::test]
async fn a_revoke_mid_backlog_counts_as_sent_only_what_reached_the_adapter() {
    let mut rig = Rig::new().await;
    rig.publish(true);
    rig.backlog(400, 1024);
    let (mut adapter, _) = rig.connect().await;
    let mut got = Vec::new();
    for _ in 0..50 {
        got.push(body_of(&adapter.outgoing().await.0));
    }
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    // A slow reader: the stream ends at the recheck, well before the backlog
    // is through.
    let (rest, status) = drain(&mut adapter, Duration::from_millis(20)).await;
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    got.extend(rest);
    assert!(got.len() < 400, "{}", got.len());
    finish_backlog(&mut rig, 400, got).await;
}

#[tokio::test]
async fn a_revoke_ends_a_stream_whose_adapter_stopped_reading() {
    let mut rig = Rig::new().await;
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
    // stream's queue fill, and the session waits for room with claims
    // queued.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rig.states().get("attempted").copied().unwrap_or(0) > 0);
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    // The session ends anyway: what it had queued goes back to pending.
    let mut states = rig.states();
    for _ in 0..500 {
        if !states.contains_key("attempted") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        states = rig.states();
    }
    assert!(!states.contains_key("attempted"), "{states:?}");
    // Once the adapter reads again, it gets exactly what was counted sent,
    // then the status.
    let (got, status) = drain(&mut adapter, Duration::ZERO).await;
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    assert!(got.len() < 100);
    finish_backlog(&mut rig, 100, got).await;
}

/// A claim queued as its queue closes is either taken for the transport or
/// handed back by the close (to return to pending), never both and never
/// lost, however the three race.
#[test]
fn a_claim_queued_as_the_stream_closes_is_sent_or_requeued_never_both() {
    use std::task::{Context, Waker};
    for round in 0..500 {
        let (tx, rx) = mpsc::channel(4);
        let queue: Queue = Arc::new(Mutex::new(Outbound { rx, closed: false }));
        let total = 64;
        let producer = {
            let queue = queue.clone();
            std::thread::spawn(move || {
                let mut refused = Vec::new();
                for claim in 0..total {
                    let permit = loop {
                        match tx.clone().try_reserve_owned() {
                            Ok(permit) => break permit,
                            Err(mpsc::error::TrySendError::Full(_)) => std::thread::yield_now(),
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                return refused_rest(refused, claim, total);
                            }
                        }
                    };
                    let item = Item {
                        frame: frame(Out::Ping(pb::Ping { nonce: 0 })),
                        claim: Some(claim),
                    };
                    if let Err(item) = enqueue(&queue, permit, item) {
                        refused.extend(item.claim);
                    }
                }
                refused
            })
        };
        let consumer = {
            let queue = queue.clone();
            std::thread::spawn(move || {
                let mut cx = Context::from_waker(Waker::noop());
                let mut taken = Vec::new();
                loop {
                    match take(&queue, &mut cx) {
                        Poll::Ready(Some(item)) => taken.extend(item.claim),
                        Poll::Ready(None) => return taken,
                        Poll::Pending => std::thread::yield_now(),
                    }
                }
            })
        };
        for _ in 0..round % 50 {
            std::thread::yield_now();
        }
        let drained = close(&queue);
        let refused = producer.join().unwrap();
        let taken = consumer.join().unwrap();
        let mut all: Vec<i64> = taken
            .iter()
            .chain(&drained)
            .chain(&refused)
            .copied()
            .collect();
        all.sort();
        assert_eq!(all, (0..total).collect::<Vec<_>>(), "round {round}");
    }
}

/// Claims a producer never got room for are, like refused ones, not sent.
fn refused_rest(mut refused: Vec<i64>, from: i64, total: i64) -> Vec<i64> {
    refused.extend(from..total);
    refused
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
    let other = integration::admin_add(&mut rig.conn(), "other").unwrap();
    let (mut adapter, _) = rig.connect().await;
    let (mut intruder, _) = rig.connect_as(&other, "other").await;
    let err = rig.send("id:will@bbs", "hi").unwrap_err();
    assert!(
        err.to_string().contains("this installation's namespace"),
        "{err:#}"
    );
    // Before `hub` is bound to anyone, observations of it are ignored.
    observe(&mut adapter, "ada", mp::Lifecycle::Retired).await;
    assert_eq!(
        rig.count("SELECT count(*) FROM entities WHERE entity_id = 'ada'"),
        0
    );
    // An import binds it to integration `hub`; from then on only `hub`
    // speaks for it.
    received(
        &adapter
            .offer(
                "h1",
                message("m1", "bob", "will/scout", "hi").encode_to_vec(),
            )
            .await,
    );
    observe(&mut intruder, "bob", mp::Lifecycle::Retired).await;
    assert_eq!(
        rig.count("SELECT count(*) FROM entities WHERE entity_id = 'bob' AND lifecycle = 'active'"),
        1
    );
    observe(&mut adapter, "ada", mp::Lifecycle::Retired).await;
    let err = rig.send("id:ada@hub:slack", "hi").unwrap_err();
    assert!(err.to_string().contains("is retired"), "{err:#}");
    assert_eq!(rig.count("SELECT count(*) FROM integration_outbox"), 0);
    assert_eq!(
        rig.count(
            "SELECT count(*) FROM entity_addresses a JOIN entities e ON e.id = a.entity_id
             WHERE e.entity_id = 'ada'"
        ),
        0
    );
    adapter.quiet().await;
}

/// Send an observation of `hub`'s `entity` and wait until it is handled.
async fn observe(adapter: &mut Adapter, entity: &str, lifecycle: mp::Lifecycle) {
    adapter
        .send(In::Observed(pb::ObservedEntities {
            entities: vec![mp::EntityDescriptor {
                entity: Some(mp::EntityRef {
                    namespace: "hub".into(),
                    entity_id: entity.into(),
                }),
                kind: "human".into(),
                display_name: entity.into(),
                lifecycle: lifecycle as i32,
                ..Default::default()
            }],
            observed_at_unix_micros: 1,
        }))
        .await;
    adapter.send(In::Ping(pb::Ping { nonce: 1 })).await;
    assert!(matches!(adapter.next().await.unwrap(), Out::Pong(_)));
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
    // Every time, the older stream ends with its status, never a bare OK.
    for _ in 0..20 {
        let (new, _) = rig.connect().await;
        assert_eq!(old.next().await.unwrap_err().code(), tonic::Code::Aborted);
        old = new;
    }
    let mut new = old;
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
    let woken = *rig.state.notify.borrow();
    let (mut adapter, _) = rig.connect().await;
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"fresh");
    adapter.quiet().await;
    assert_eq!(rig.outbox(claimed).0, "rejected");
    assert!(notice_body(&rig, claimed).is_some());
    // That notice woke the sender's waiting reads.
    assert!(*rig.state.notify.borrow() > woken);
    rig.settle(fresh, "sent").await;
    assert!(notice_body(&rig, fresh).is_none());

    // Sent work the adapter never answers by its deadline is uncertain.
    assert_eq!(
        integration::expire(&mut rig.conn(), now + chrono::Duration::days(8)).unwrap(),
        1
    );
    let (state, detail) = rig.outbox(fresh);
    assert_eq!(state, "unknown");
    assert_eq!(
        detail.as_deref(),
        Some("no result from the adapter by the deadline")
    );
    assert!(
        notice_body(&rig, fresh)
            .unwrap()
            .starts_with("Delivery uncertain: ")
    );

    // Notices are BBS's, not the agent's own sends: they move neither its
    // last send nor a search by sender.
    let fresh_at: String = rig
        .conn()
        .query_row(
            "SELECT created_at FROM messages WHERE id = ?1",
            [fresh],
            |r| r.get(0),
        )
        .unwrap();
    let scout = db::list_agents(&rig.conn(), false)
        .unwrap()
        .into_iter()
        .find(|a| a.name == "scout")
        .unwrap();
    assert_eq!(scout.last_sent_at.as_deref(), Some(fresh_at.as_str()));
    let search = |from: Option<&str>| {
        service::search(
            &mut rig.conn(),
            &human(),
            proto::SearchRequest {
                query: "delivered".into(),
                from: from.map(Into::into),
                ..Default::default()
            },
        )
        .unwrap()
        .hits
        .len()
    };
    assert!(search(None) > 0);
    assert_eq!(search(Some("scout")), 0);
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
    // So may a group copy whose original is in a namespace `hub` does not
    // own, even when the copy comes from `hub`'s own namespace.
    let mut copy = message("c1", "team", "will/scout", "copied");
    copy.group_copy = Some(mp::GroupCopy {
        original: Some(mp::MessageRef {
            namespace: "acme".into(),
            message_id: "orig".into(),
        }),
        original_sender: address("acme", "eve"),
    });
    rejected(
        &hub.offer("h3", copy.encode_to_vec()).await,
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

// ---- remote directories ------------------------------------------------------

/// A client of the rig's main gRPC service, acting as `caller`.
async fn client(rig: &Rig, caller: Caller) -> crate::backend::Backend {
    use crate::proto::bbs_server::BbsServer;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BbsServer::new(super::super::Server(rig.state.clone())))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    crate::backend::Backend::Remote(
        crate::backend::Remote::connect(&addr, caller)
            .await
            .unwrap(),
    )
}

fn agent(rig: &Rig) -> Caller {
    Caller {
        token: Some(rig.agent.clone()),
        ..Default::default()
    }
}

fn list(namespace: &str) -> proto::RemoteDirectoryRequest {
    proto::RemoteDirectoryRequest {
        namespace: namespace.into(),
        ..Default::default()
    }
}

/// Ask in the background; the test plays the adapter meanwhile.
fn ask_remote(
    mut b: crate::backend::Backend,
    req: proto::RemoteDirectoryRequest,
) -> tokio::task::JoinHandle<anyhow::Result<proto::RemoteDirectoryResponse>> {
    tokio::spawn(async move { b.remote_directory(req).await })
}

/// The next frame, which must be a remote directory query.
async fn remote_query(adapter: &mut Adapter) -> mp::DirectoryQuery {
    match adapter.next().await.unwrap() {
        Out::RemoteDirectoryQuery(q) => q,
        other => panic!("expected a remote directory query, got {other:?}"),
    }
}

fn descriptor(namespace: &str, id: &str, kind: &str) -> mp::EntityDescriptor {
    mp::EntityDescriptor {
        entity: Some(mp::EntityRef {
            namespace: namespace.into(),
            entity_id: id.into(),
        }),
        kind: kind.into(),
        display_name: id.to_uppercase(),
        aliases: vec![id.into()],
        lifecycle: mp::Lifecycle::Active as i32,
        ..Default::default()
    }
}

fn code_of(err: &anyhow::Error) -> tonic::Code {
    err.downcast_ref::<Status>()
        .unwrap_or_else(|| panic!("not a status: {err:#}"))
        .code()
}

/// The caller's query reaches the adapter as its own entity, and the
/// verified page comes back, without descriptors from elsewhere.
#[tokio::test]
async fn remote_directories_are_asked_as_the_caller_and_answered() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let asked = ask_remote(
        client(&rig, human()).await,
        proto::RemoteDirectoryRequest {
            page_size: 500,
            page_token: b"next".to_vec(),
            ..list("hub")
        },
    );
    let q = remote_query(&mut adapter).await;
    let who = q.requester.clone().unwrap().entity.unwrap();
    assert_eq!(
        (who.namespace.as_str(), who.entity_id.as_str()),
        ("bbs", "will")
    );
    assert_eq!(q.namespace, "hub");
    assert_eq!((q.alias.as_str(), q.entity_id.as_str()), ("", ""));
    assert_eq!(q.page_size, 100, "capped at the largest page");
    assert_eq!(q.page_token, b"next");
    let left = q.deadline_unix_micros - Utc::now().timestamp_micros();
    assert!((4_000_000..=5_000_000).contains(&left), "{left}");
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: q.query_id,
            entities: vec![
                descriptor("hub", "ada", "human"),
                descriptor("acme", "mallory", "human"),
                descriptor("hub", "echo", "agent"),
            ],
            next_page_token: b"more".to_vec(),
            ..Default::default()
        }))
        .await;
    let page = asked.await.unwrap().unwrap();
    let ids: Vec<_> = page
        .entities
        .iter()
        .map(|e| format!("{}@{} {} {}", e.entity_id, e.namespace, e.kind, e.lifecycle))
        .collect();
    assert_eq!(ids, ["ada@hub human active", "echo@hub agent active"]);
    assert_eq!(page.entities[0].display_name, "ADA");
    assert_eq!(page.next_page_token, b"more");

    // An agent asks as its own entity; an alias is normalized.
    let asked = ask_remote(
        client(&rig, agent(&rig)).await,
        proto::RemoteDirectoryRequest {
            alias: "Ada".into(),
            ..list("hub")
        },
    );
    let q = remote_query(&mut adapter).await;
    let who = q.requester.clone().unwrap().entity.unwrap();
    assert_eq!(who.entity_id, "will/scout");
    assert_eq!(q.alias, "ada");
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: q.query_id,
            ..Default::default()
        }))
        .await;
    assert!(asked.await.unwrap().unwrap().entities.is_empty());
    let q = rig.state.integrations.lock().unwrap()["hub"]
        .queries()
        .as_ref()
        .unwrap()
        .len();
    assert_eq!(q, 0, "no waiter is left behind");
}

/// A remote home's failure is an error carrying its reason, never an empty
/// directory.
#[tokio::test]
async fn remote_directory_failures_pass_through_as_errors() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    use tonic::Code::*;
    for (reason, code) in [
        ("namespace_unknown", NotFound),
        ("entity_unknown", NotFound),
        ("invalid_cursor", InvalidArgument),
        ("invalid_argument", InvalidArgument),
        ("invalid_address", InvalidArgument),
        ("permission_denied", PermissionDenied),
        ("route_not_authorized", PermissionDenied),
        ("invalid_signature", PermissionDenied),
        ("expired", DeadlineExceeded),
        ("directory_timeout", DeadlineExceeded),
        ("queue_full", ResourceExhausted),
        // A remote home cannot judge the caller's own credentials.
        ("invalid_token", Unavailable),
        ("token_revoked", Unavailable),
        ("token_unknown", Unavailable),
        ("scope_denied", Unavailable),
        ("home_offline", Unavailable),
        ("relay_unavailable", Unavailable),
        ("invalid_message", Unavailable),
    ] {
        let asked = ask_remote(client(&rig, human()).await, list("hub"));
        let q = remote_query(&mut adapter).await;
        adapter
            .send(In::RemoteDirectoryResult(mp::DirectoryResult {
                query_id: q.query_id,
                error_code: reason.into(),
                detail: "said the\nother home".into(),
                entities: vec![descriptor("hub", "ada", "human")],
                ..Default::default()
            }))
            .await;
        let err = asked.await.unwrap().unwrap_err();
        assert_eq!(code_of(&err), code, "{reason}");
        assert_eq!(
            err.to_string(),
            format!("{reason}: said the other home"),
            "{reason}"
        );
    }
}

/// An adapter that does not answer in time, or whose stream ends, fails
/// the query as unavailable; its late answer is ignored.
#[tokio::test]
async fn unanswered_remote_directories_time_out() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let asked = ask_remote(client(&rig, human()).await, list("hub"));
    let q = remote_query(&mut adapter).await;
    let err = timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::DeadlineExceeded);
    assert!(err.to_string().starts_with("directory_timeout:"), "{err:#}");
    let waiting = rig.state.integrations.lock().unwrap()["hub"]
        .queries()
        .as_ref()
        .unwrap()
        .len();
    assert_eq!(waiting, 0, "the timed-out waiter is gone");
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: q.query_id,
            ..Default::default()
        }))
        .await;
    // The stream carries on.
    adapter.send(In::Ping(pb::Ping { nonce: 7 })).await;
    let Out::Pong(pong) = adapter.next().await.unwrap() else {
        panic!("expected a pong");
    };
    assert_eq!(pong.nonce, 7);

    // A stream that ends while a query waits fails it at once.
    let asked = ask_remote(client(&rig, human()).await, list("hub"));
    remote_query(&mut adapter).await;
    drop(adapter);
    let err = timeout(Duration::from_secs(3), asked)
        .await
        .expect("failed before the timeout")
        .unwrap()
        .unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::Unavailable);
    assert!(err.to_string().starts_with("relay_unavailable:"), "{err:#}");
}

/// Refusals come before anything reaches an adapter.
#[tokio::test]
async fn remote_directories_are_refused_without_a_usable_integration() {
    let rig = Rig::new().await;
    let mut b = client(&rig, human()).await;
    // Publication off: the caller has no entity Hub could know.
    let err = b.remote_directory(list("hub")).await.unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::FailedPrecondition);
    assert!(err.to_string().contains("does not publish"), "{err:#}");
    // On, but no adapter connected.
    rig.publish(true);
    let err = b.remote_directory(list("hub")).await.unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::Unavailable);
    assert!(err.to_string().contains("is not connected"), "{err:#}");
    // This installation's own namespace, and bad selectors.
    let (mut adapter, _) = rig.connect().await;
    for req in [
        list("bbs"),
        list("Not A Namespace"),
        proto::RemoteDirectoryRequest {
            alias: "ada".into(),
            entity_id: "ada".into(),
            ..list("hub")
        },
        proto::RemoteDirectoryRequest {
            alias: "no spaces".into(),
            ..list("hub")
        },
        proto::RemoteDirectoryRequest {
            entity_id: "a/b/c".into(),
            ..list("hub")
        },
    ] {
        let err = b.remote_directory(req.clone()).await.unwrap_err();
        assert_eq!(
            code_of(&err),
            tonic::Code::InvalidArgument,
            "{req:?}: {err:#}"
        );
    }
    adapter.quiet().await;
    // No integration enabled at all.
    drop(adapter);
    assert!(integration::admin_revoke(&mut rig.conn(), "hub").unwrap());
    let err = b.remote_directory(list("hub")).await.unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::FailedPrecondition);
    assert!(
        err.to_string().contains("no integration is enabled"),
        "{err:#}"
    );
}

fn waiting(rig: &Rig) -> usize {
    rig.state.integrations.lock().unwrap()["hub"]
        .queries()
        .as_ref()
        .unwrap()
        .len()
}

/// Hold `n` places among the stream's waiting queries for `caller`, as
/// queries the adapter has not answered would.
fn occupy(
    rig: &Rig,
    caller: &str,
    n: usize,
) -> Vec<oneshot::Receiver<Result<mp::DirectoryResult, Status>>> {
    let sessions = rig.state.integrations.lock().unwrap();
    let mut queries = sessions["hub"].queries();
    let waiting = queries.as_mut().unwrap();
    (0..n)
        .map(|i| {
            let (done, rx) = oneshot::channel();
            waiting.insert(
                format!("{caller}-{i}").replace('/', "-"),
                Query {
                    caller: caller.into(),
                    done,
                },
            );
            rx
        })
        .collect()
}

/// Too many queries waiting on the stream, or for one caller, is
/// `RESOURCE_EXHAUSTED` before anything reaches the adapter; another caller
/// still gets through.
#[tokio::test]
async fn remote_directory_queries_are_bounded_per_stream_and_per_caller() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let mut will = client(&rig, human()).await;
    let held = occupy(&rig, "will", MAX_REMOTE_QUERIES_PER_CALLER);
    let err = will.remote_directory(list("hub")).await.unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::ResourceExhausted, "{err:#}");
    assert!(err.to_string().starts_with("queue_full: "), "{err:#}");
    // The agent is another caller.
    let asked = ask_remote(client(&rig, agent(&rig)).await, list("hub"));
    let q = remote_query(&mut adapter).await;
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: q.query_id,
            ..Default::default()
        }))
        .await;
    asked.await.unwrap().unwrap();
    drop(held);
    // The stream's own bound, whoever asks.
    let _held = occupy(&rig, "others", MAX_REMOTE_QUERIES - waiting(&rig));
    let err = will.remote_directory(list("hub")).await.unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::ResourceExhausted, "{err:#}");
    assert!(err.to_string().contains("integration hub"), "{err:#}");
    adapter.quiet().await;
}

/// Answers find their own queries in whatever order they come, and what a
/// page shows is cached as observed, at the server's clock.
#[tokio::test]
async fn remote_directory_answers_out_of_order_and_are_observed() {
    let rig = Rig::new().await;
    rig.publish(true);
    // Observations count for a namespace bound to the integration.
    db::integration::bind_namespace(&rig.conn(), "hub", "hub").unwrap();
    let (mut adapter, _) = rig.connect().await;
    let first = ask_remote(
        client(&rig, human()).await,
        proto::RemoteDirectoryRequest {
            alias: "ada".into(),
            ..list("hub")
        },
    );
    let q1 = remote_query(&mut adapter).await;
    let second = ask_remote(
        client(&rig, agent(&rig)).await,
        proto::RemoteDirectoryRequest {
            entity_id: " echo ".into(),
            ..list("hub")
        },
    );
    let q2 = remote_query(&mut adapter).await;
    assert_eq!(q2.entity_id, "echo", "trimmed");
    for (q, id, kind) in [(&q2, "echo", "agent"), (&q1, "ada", "human")] {
        adapter
            .send(In::RemoteDirectoryResult(mp::DirectoryResult {
                query_id: q.query_id.clone(),
                entities: vec![descriptor("hub", id, kind)],
                ..Default::default()
            }))
            .await;
    }
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!(first.entities[0].entity_id, "ada");
    assert_eq!(second.entities[0].entity_id, "echo");
    assert_eq!(
        rig.count(
            "SELECT count(*) FROM entities WHERE namespace = 'hub'
             AND ((entity_id = 'ada' AND kind = 'human' AND display_name = 'ADA')
               OR (entity_id = 'echo' AND kind = 'agent'))"
        ),
        2
    );
}

/// A malformed result fails only its own query; one naming no query is
/// dropped; the stream and its handoffs carry on.
#[tokio::test]
async fn a_malformed_remote_directory_result_fails_only_its_query() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut adapter, _) = rig.connect().await;
    let too_many = |id: String| mp::DirectoryResult {
        query_id: id,
        entities: (0..101)
            .map(|i| descriptor("hub", &format!("e{i}"), "human"))
            .collect(),
        ..Default::default()
    };
    let long_token = |id: String| mp::DirectoryResult {
        query_id: id,
        next_page_token: vec![b'x'; 4097],
        ..Default::default()
    };
    for malformed in [
        &too_many as &dyn Fn(String) -> mp::DirectoryResult,
        &long_token,
    ] {
        let asked = ask_remote(client(&rig, human()).await, list("hub"));
        let q = remote_query(&mut adapter).await;
        adapter
            .send(In::RemoteDirectoryResult(malformed(q.query_id)))
            .await;
        let err = timeout(Duration::from_secs(3), asked)
            .await
            .expect("failed at once")
            .unwrap()
            .unwrap_err();
        assert_eq!(code_of(&err), tonic::Code::Unavailable, "{err:#}");
        assert!(err.to_string().starts_with("invalid_message: "), "{err:#}");
    }
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: "not an id".into(),
            ..Default::default()
        }))
        .await;
    // The stream carries on: a handoff still goes out.
    let id = rig.send("id:ada@hub", "still here").unwrap().id.unwrap();
    let (_, m) = adapter.outgoing().await;
    assert_eq!(m.body, b"still here");
    rig.settle(id, "sent").await;
}

/// A newer stream supersedes the one a query waits on: the query fails at
/// once, and the new stream serves the next.
#[tokio::test]
async fn a_superseded_stream_fails_its_waiting_queries() {
    let rig = Rig::new().await;
    rig.publish(true);
    let (mut old, _) = rig.connect().await;
    let asked = ask_remote(client(&rig, human()).await, list("hub"));
    let q = remote_query(&mut old).await;
    let (mut new, _) = rig.connect().await;
    let err = timeout(Duration::from_secs(3), asked)
        .await
        .expect("failed at once")
        .unwrap()
        .unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::Unavailable, "{err:#}");
    assert!(err.to_string().starts_with("relay_unavailable:"), "{err:#}");
    // The old stream's late answer reaches no one.
    let _ = old
        .tx
        .send(pb::AdapterFrame {
            payload: Some(In::RemoteDirectoryResult(mp::DirectoryResult {
                query_id: q.query_id,
                ..Default::default()
            })),
        })
        .await;
    let asked = ask_remote(client(&rig, human()).await, list("hub"));
    let q = remote_query(&mut new).await;
    new.send(In::RemoteDirectoryResult(mp::DirectoryResult {
        query_id: q.query_id,
        entities: vec![descriptor("hub", "ada", "human")],
        ..Default::default()
    }))
    .await;
    assert_eq!(asked.await.unwrap().unwrap().entities.len(), 1);
}

/// A focused agent may not query remote directories, as it may not send
/// remotely; its owner still can.
#[tokio::test]
async fn a_focused_agent_may_not_query_remote_directories() {
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
    let err = client(&rig, agent(&rig))
        .await
        .remote_directory(list("hub"))
        .await
        .unwrap_err();
    assert_eq!(code_of(&err), tonic::Code::FailedPrecondition, "{err:#}");
    assert!(err.to_string().contains("focus mode"), "{err:#}");
    adapter.quiet().await;
    let asked = ask_remote(client(&rig, human()).await, list("hub"));
    let q = remote_query(&mut adapter).await;
    adapter
        .send(In::RemoteDirectoryResult(mp::DirectoryResult {
            query_id: q.query_id,
            ..Default::default()
        }))
        .await;
    asked.await.unwrap().unwrap();
}
