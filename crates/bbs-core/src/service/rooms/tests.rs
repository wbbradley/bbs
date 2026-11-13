//! Rooms against a fake host: an in-memory room log answering room requests
//! as Hub does (cursors with an epoch, AFTER/LATEST/BEFORE/REFS reads,
//! filters, idempotent changes), behind a [`RoomHome`] whose clock the test
//! moves. Posts leave through the real outbox and reach the fake host when
//! the test hands them off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};
use prost::Message as _;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::super::{Caller, integration};
use super::*;
use crate::hub_proto::integration as pb;
use crate::hub_proto::msgproto_v1 as mp;
use crate::{db, proto};

/// One room on the fake host.
#[derive(Default)]
struct HostRoom {
    info: mp::RoomInfo,
    /// (position, message, accepted at)
    log: Vec<(u64, mp::Message, i64)>,
    refused: HashMap<(String, String), String>,
}

#[derive(Default)]
struct Host {
    ns: String,
    rooms: HashMap<String, HostRoom>,
    next: u64,
    epoch: u64,
    ops: HashMap<(String, String), mp::RoomAnswer>,
    /// Answer nothing: every request is RoomUnavailable.
    down: bool,
    requests: Vec<mp::RoomRequest>,
}

fn cursor(room: &str, position: u64, epoch: u64) -> Vec<u8> {
    format!("{room}|{position}|{epoch}").into_bytes()
}

impl Host {
    fn new() -> Host {
        Host {
            ns: "hub".into(),
            next: 1,
            epoch: 1,
            ..Default::default()
        }
    }

    fn create(&mut self, name: &str, creator: mp::EntityRef) {
        let path = format!("rooms/{name}");
        let info = mp::RoomInfo {
            room: Some(mp::EntityRef {
                namespace: self.ns.clone(),
                entity_id: path.clone(),
            }),
            name: path.clone(),
            post: vec![mp::RoomSubject {
                subject: Some(mp::room_subject::Subject::Any(true)),
            }],
            managers: vec![mp::RoomSubject {
                subject: Some(mp::room_subject::Subject::Entity(creator.clone())),
            }],
            revision: 1,
            creator: Some(creator),
            address: Some(mp::AddressRef {
                entity: Some(mp::EntityRef {
                    namespace: self.ns.clone(),
                    entity_id: path.clone(),
                }),
                address_id: "native".into(),
                run_id: None,
            }),
            ..Default::default()
        };
        self.rooms.insert(
            path,
            HostRoom {
                info,
                ..Default::default()
            },
        );
    }

    /// Append a post (or the room's notice) to a room's log.
    fn append(&mut self, room: &str, m: mp::Message, at: i64) -> u64 {
        let p = self.next;
        self.next += 1;
        self.rooms.get_mut(room).unwrap().log.push((p, m, at));
        p
    }

    /// A post by `author` (PATH@NS) to `room`.
    fn post(&mut self, room: &str, author: &str, body: &str, at: i64) -> mp::MessageRef {
        let (path, ns) = author.split_once('@').unwrap();
        let id = mp::MessageRef {
            namespace: ns.into(),
            message_id: db::new_message_id(),
        };
        let m = mp::Message {
            id: Some(id.clone()),
            sender: Some(addr(ns, path)),
            recipient: Some(addr(&self.ns, room)),
            authored_at_unix_micros: at,
            expires_at_unix_micros: Some(at + 1_000_000_000),
            content_type: "text/plain; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            kind: mp::MessageKind::Application as i32,
            ..Default::default()
        };
        self.append(room, m, at);
        id
    }

    /// Restore: a new epoch, and the log replaced by `keep` of its first posts.
    fn restore(&mut self, keep: usize) {
        self.epoch += 1;
        for r in self.rooms.values_mut() {
            r.log.truncate(keep);
        }
    }

    fn record(&self, room: &str, (p, m, at): &(u64, mp::Message, i64)) -> mp::RoomRecord {
        mp::RoomRecord {
            message: Some(mp::EncodedMessage {
                format_major: 1,
                message_bytes: m.encode_to_vec(),
            }),
            position: *p,
            cursor: cursor(room, *p, self.epoch),
            accepted_at_unix_micros: *at,
        }
    }

    fn answer(&mut self, req: mp::RoomRequest) -> pb::RoomResult {
        self.requests.push(req.clone());
        if self.down {
            return pb::RoomResult {
                request_id: req.request_id,
                unavailable_code: "home_offline".into(),
                detail: "the host is down".into(),
                answer: None,
            };
        }
        let mut a = mp::RoomAnswer {
            namespace: self.ns.clone(),
            request_id: req.request_id.clone(),
            ..Default::default()
        };
        let who = req.requester.clone().unwrap().entity.unwrap();
        let fail = |a: &mut mp::RoomAnswer, code: &str| a.error_code = code.into();
        match req.op.clone().unwrap() {
            mp::room_request::Op::List(l) => {
                let mut rooms: Vec<mp::RoomInfo> = self
                    .rooms
                    .values()
                    .filter(|r| l.include_archived || !r.info.archived)
                    .filter(|r| r.info.name.contains(&l.filter))
                    .map(|r| r.info.clone())
                    .collect();
                rooms.sort_by(|a, b| a.name.cmp(&b.name));
                a.result = Some(mp::room_answer::Result::List(mp::RoomListResult {
                    rooms,
                    next_page_token: Vec::new(),
                }));
            }
            mp::room_request::Op::Create(c) => {
                let key = (
                    format!("{}/{}", who.namespace, who.entity_id),
                    c.operation_id.clone(),
                );
                if let Some(prior) = self.ops.get(&key) {
                    let mut again = prior.clone();
                    again.request_id = req.request_id.clone();
                    return ok(again);
                }
                let path = format!("rooms/{}", c.name);
                if let Some(r) = self.rooms.get(&path) {
                    fail(&mut a, "conflict");
                    a.result = Some(mp::room_answer::Result::Info(Box::new(r.info.clone())));
                } else {
                    self.create(&c.name, who.clone());
                    let r = self.rooms.get_mut(&path).unwrap();
                    r.info.description = c.description;
                    r.info.post = c.post;
                    let mut managers = c.managers;
                    let me = mp::RoomSubject {
                        subject: Some(mp::room_subject::Subject::Entity(who.clone())),
                    };
                    if !managers.contains(&me) {
                        managers.insert(0, me);
                    }
                    r.info.managers = managers;
                    a.result = Some(mp::room_answer::Result::Info(Box::new(r.info.clone())));
                }
                self.ops.insert(key, a.clone());
            }
            mp::room_request::Op::Info(_) => match self.rooms.get(&req.room) {
                Some(r) => {
                    let mut info = r.info.clone();
                    info.head_position = r.log.last().map_or(0, |l| l.0);
                    info.head_cursor = cursor(&req.room, info.head_position, self.epoch);
                    a.result = Some(mp::room_answer::Result::Info(Box::new(info)));
                }
                None => fail(&mut a, "not_found"),
            },
            mp::room_request::Op::Update(u) => {
                let Some(r) = self.rooms.get_mut(&req.room) else {
                    fail(&mut a, "not_found");
                    return ok(a);
                };
                if u.expected_revision != r.info.revision {
                    fail(&mut a, "revision_mismatch");
                    return ok(a);
                }
                if let Some(d) = u.description {
                    r.info.description = d;
                }
                if u.set_post {
                    r.info.post = u.post;
                }
                if u.set_managers {
                    r.info.managers = u.managers;
                }
                if let Some(x) = u.archived {
                    r.info.archived = x;
                }
                r.info.revision += 1;
                a.result = Some(mp::room_answer::Result::Info(Box::new(r.info.clone())));
            }
            mp::room_request::Op::Read(rd) => {
                let Some(r) = self.rooms.get(&req.room) else {
                    fail(&mut a, "not_found");
                    return ok(a);
                };
                let head = r.log.last().map_or(0, |l| l.0);
                let mut page = mp::RoomPage {
                    head_position: head,
                    head_cursor: cursor(&req.room, head, self.epoch),
                    ..Default::default()
                };
                let parse = |c: &[u8]| -> Option<u64> {
                    if c.is_empty() {
                        return Some(0);
                    }
                    let s = String::from_utf8(c.to_vec()).ok()?;
                    let mut it = s.split('|');
                    let (room, pos, epoch) = (it.next()?, it.next()?, it.next()?);
                    let (pos, epoch): (u64, u64) = (pos.parse().ok()?, epoch.parse().ok()?);
                    (room == req.room && epoch == self.epoch && pos <= head).then_some(pos)
                };
                let matches = |(_, m, at): &&(u64, mp::Message, i64)| {
                    (rd.text.is_empty()
                        || String::from_utf8_lossy(&m.body).contains(rd.text.as_str()))
                        && rd.author.as_ref().is_none_or(|e| {
                            m.sender.as_ref().and_then(|s| s.entity.as_ref()) == Some(e)
                        })
                        && rd.since_unix_micros.is_none_or(|t| *at >= t)
                        && rd.until_unix_micros.is_none_or(|t| *at <= t)
                        && rd
                            .in_reply_to
                            .as_ref()
                            .is_none_or(|p| m.in_reply_to.as_ref() == Some(p))
                };
                let limit = if rd.limit == 0 { 50 } else { rd.limit as usize };
                match mp::RoomReadMode::try_from(rd.mode).unwrap() {
                    mp::RoomReadMode::After => {
                        let Some(from) = parse(&rd.cursor) else {
                            fail(&mut a, "cursor_invalid");
                            return ok(a);
                        };
                        let after: Vec<_> =
                            r.log.iter().filter(|l| l.0 > from).take(limit).collect();
                        page.next_cursor = match after.last() {
                            Some(l) => cursor(&req.room, l.0, self.epoch),
                            None => rd.cursor.clone(),
                        };
                        page.records = after
                            .iter()
                            .copied()
                            .filter(matches)
                            .map(|l| self.record(&req.room, l))
                            .collect();
                    }
                    mode @ (mp::RoomReadMode::Latest | mp::RoomReadMode::Before) => {
                        let below = if mode == mp::RoomReadMode::Before {
                            match parse(&rd.cursor) {
                                Some(p) => p,
                                None => {
                                    fail(&mut a, "cursor_invalid");
                                    return ok(a);
                                }
                            }
                        } else {
                            u64::MAX
                        };
                        let mut found: Vec<_> = r
                            .log
                            .iter()
                            .rev()
                            .filter(|l| l.0 < below)
                            .filter(matches)
                            .take(limit)
                            .collect();
                        found.reverse();
                        if let Some(first) = found.first() {
                            page.next_cursor = cursor(&req.room, first.0, self.epoch);
                        }
                        page.records = found.iter().map(|l| self.record(&req.room, l)).collect();
                    }
                    mp::RoomReadMode::Refs => {
                        for want in &rd.refs {
                            let hit = r.log.iter().find(|l| l.1.id.as_ref() == Some(want));
                            let key = (want.namespace.clone(), want.message_id.clone());
                            let (status, reason) = match (hit, r.refused.get(&key)) {
                                (Some(l), _) => {
                                    page.records.push(self.record(&req.room, l));
                                    (mp::RoomRefStatus::Present, String::new())
                                }
                                (None, Some(why)) => (mp::RoomRefStatus::Refused, why.clone()),
                                (None, None) => (mp::RoomRefStatus::Absent, String::new()),
                            };
                            page.refs.push(mp::RoomRefResult {
                                r#ref: Some(want.clone()),
                                status: status as i32,
                                reason,
                            });
                        }
                    }
                    _ => fail(&mut a, "invalid"),
                }
                if a.error_code.is_empty() {
                    a.result = Some(mp::room_answer::Result::Page(page));
                }
            }
        }
        ok(a)
    }
}

fn ok(a: mp::RoomAnswer) -> pb::RoomResult {
    pb::RoomResult {
        request_id: a.request_id.clone(),
        answer: Some(a),
        ..Default::default()
    }
}

fn addr(ns: &str, path: &str) -> mp::AddressRef {
    mp::AddressRef {
        entity: Some(mp::EntityRef {
            namespace: ns.into(),
            entity_id: path.into(),
        }),
        address_id: "native".into(),
        run_id: None,
    }
}

struct Home {
    conn: Mutex<Connection>,
    host: Mutex<Host>,
    clock: Mutex<DateTime<Utc>>,
    changes: Mutex<u64>,
}

impl RoomHome for Home {
    async fn db<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        f(&mut self.conn.lock().unwrap())
    }

    async fn ask(&self, integration: String, request: mp::RoomRequest) -> pb::RoomResult {
        assert_eq!(integration, "hub");
        let id = request.request_id.clone();
        let mut r = self.host.lock().unwrap().answer(request);
        r.request_id = id;
        r
    }

    fn changed(&self) {
        *self.changes.lock().unwrap() += 1;
    }

    fn now(&self) -> DateTime<Utc> {
        *self.clock.lock().unwrap()
    }
}

/// A board with human `will` and agents `scout` and `ranger`
/// (`will/scout@bbs`, `will/ranger@bbs`), integration `hub` publishing, and
/// a fake host `hub`.
struct Rig {
    _dir: tempfile::TempDir,
    home: Arc<Home>,
    cfg: RoomConfig,
    scout: Caller,
    ranger: Caller,
}

fn agent(token: String) -> Caller {
    Caller {
        token: Some(token),
        ..Default::default()
    }
}

impl Rig {
    fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("bbs.db")).unwrap();
        db::migrate(&mut conn).unwrap();
        super::super::ensure_human(
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
        let mut reg = |name: &str| {
            super::super::register(
                &mut conn,
                &will,
                proto::RegisterRequest {
                    name: Some(name.into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .token
        };
        let (scout, ranger) = (reg("scout"), reg("ranger"));
        integration::admin_add(&mut conn, "hub").unwrap();
        integration::admin_publish(&mut conn, "hub", true).unwrap();
        let home = Arc::new(Home {
            conn: Mutex::new(conn),
            host: Mutex::new(Host::new()),
            clock: Mutex::new(Utc::now()),
            changes: Mutex::new(0),
        });
        Rig {
            _dir: dir,
            home,
            cfg: RoomConfig {
                jitter: 0.0,
                ..Default::default()
            },
            scout: agent(scout),
            ranger: agent(ranger),
        }
    }

    fn now(&self) -> i64 {
        self.home.now().timestamp_micros()
    }

    fn advance(&self, secs: i64) {
        *self.home.clock.lock().unwrap() += chrono::Duration::seconds(secs);
    }

    async fn run(&self, who: &Caller, verb: &str, args: Value) -> Result<Value> {
        command(&*self.home, &self.cfg, who, verb, &args.to_string()).await
    }

    async fn poll(&self) {
        poll_once(&*self.home, &self.cfg).await.unwrap();
    }

    /// Poll at intervals until nothing changes for a while, moving the clock
    /// a quiet poll interval each time.
    async fn settle(&self) {
        for _ in 0..4 {
            self.poll().await;
            self.advance(61);
        }
        self.poll().await;
    }

    fn host(&self) -> std::sync::MutexGuard<'_, Host> {
        self.home.host.lock().unwrap()
    }

    fn read(&self, who: &Caller) -> Vec<proto::Message> {
        super::super::read(
            &mut self.home.conn.lock().unwrap(),
            who,
            proto::ReadRequest::default(),
        )
        .unwrap()
        .messages
    }

    fn read_waiting(&self, who: &Caller) -> Vec<proto::Message> {
        super::super::read_mail(
            &mut self.home.conn.lock().unwrap(),
            who,
            proto::ReadRequest::default(),
            true,
        )
        .unwrap()
        .messages
    }

    /// Hand off every pending room post to the fake host, as the adapter and
    /// relay would: appended, or refused with `refuse`'s reason.
    fn hand_off(&self, refuse: Option<&str>) {
        let mut conn = self.home.conn.lock().unwrap();
        loop {
            let claim = integration::claim(&mut conn, "hub").unwrap();
            let Some((outbox, offer)) = claim.handoff else {
                break;
            };
            integration::handed_off(&conn, outbox).unwrap();
            let m = mp::Message::decode(offer.message.unwrap().message_bytes.as_slice()).unwrap();
            let to = m.recipient.clone().unwrap().entity.unwrap();
            let mut host = self.home.host.lock().unwrap();
            if let Some(r) = host.rooms.get_mut(&to.entity_id) {
                let id = m.id.clone().unwrap();
                match refuse {
                    Some(why) => {
                        r.refused.insert((id.namespace, id.message_id), why.into());
                    }
                    None => {
                        let at = self.now();
                        host.append(&to.entity_id, m, at);
                    }
                }
            }
            drop(host);
            integration::record_result(
                &mut conn,
                "hub",
                &pb::HandoffResult {
                    handoff_id: offer.handoff_id,
                    disposition: pb::HandoffDisposition::Received as i32,
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }

    /// Messages waiting in the outbox (tags), decoded.
    fn outbox(&self) -> Vec<mp::Message> {
        let conn = self.home.conn.lock().unwrap();
        conn.prepare(
            "SELECT m.encoded_message FROM integration_outbox o JOIN messages m
             ON m.id = o.message_id WHERE o.state = 'pending'",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|b| mp::Message::decode(b.unwrap().as_slice()).unwrap())
        .collect()
    }
}

fn scout_entity() -> mp::EntityRef {
    mp::EntityRef {
        namespace: "bbs".into(),
        entity_id: "will/scout".into(),
    }
}

#[tokio::test]
async fn requests_carry_the_caller_and_answers_come_back() {
    let rig = Rig::new();
    rig.host().create("incident", scout_entity());
    let info = rig
        .run(&rig.scout, "info", json!({"room": "incident"}))
        .await
        .unwrap();
    assert_eq!(info["room"], "rooms/incident@hub");
    assert_eq!(info["managers"], json!(["entity:will/scout@bbs"]));
    let req = rig.host().requests.last().cloned().unwrap();
    assert_eq!(req.namespace, "hub");
    assert_eq!(req.room, "rooms/incident");
    assert_eq!(req.requester.unwrap().entity.unwrap(), scout_entity());
    let listed = rig
        .run(&rig.ranger, "list", json!({"filter": "inc"}))
        .await
        .unwrap();
    assert_eq!(listed["rooms"][0]["name"], "incident");
    // A host error comes back as its code.
    let err = rig
        .run(&rig.scout, "info", json!({"room": "rooms/nope@hub"}))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("not_found"), "{err:#}");
}

#[tokio::test]
async fn create_reuses_its_operation_until_answered_and_conflict_is_fine_for_a_manager() {
    let rig = Rig::new();
    rig.host().down = true;
    let err = rig
        .run(
            &rig.scout,
            "create",
            json!({"name": "war-room", "desc": "x"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("home_offline"), "{err:#}");
    let first = match rig.host().requests.last().unwrap().op.clone().unwrap() {
        mp::room_request::Op::Create(c) => c.operation_id,
        _ => unreachable!(),
    };
    rig.host().down = false;
    let made = rig
        .run(
            &rig.scout,
            "create",
            json!({"name": "war-room", "desc": "x"}),
        )
        .await
        .unwrap();
    assert_eq!(made["created"], true);
    let second = match rig.host().requests.last().unwrap().op.clone().unwrap() {
        mp::room_request::Op::Create(c) => c.operation_id,
        _ => unreachable!(),
    };
    assert_eq!(first, second, "a rerun reuses the pending operation ID");
    assert_eq!(made["post"], json!(["any"]), "the CLI default is any");
    // Answered: the same command again is a new operation, and the room
    // exists and scout manages it.
    let again = rig
        .run(
            &rig.scout,
            "create",
            json!({"name": "war-room", "desc": "x"}),
        )
        .await
        .unwrap();
    assert_eq!(again["created"], false);
    let err = rig
        .run(&rig.ranger, "create", json!({"name": "war-room"}))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("conflict"), "{err:#}");
}

#[tokio::test]
async fn managers_update_with_the_current_revision() {
    let rig = Rig::new();
    rig.run(&rig.scout, "create", json!({"name": "ops"}))
        .await
        .unwrap();
    let v = rig
        .run(
            &rig.scout,
            "policy",
            json!({"room": "ops", "post": ["ns:bbs", "ns:lab"]}),
        )
        .await
        .unwrap();
    assert_eq!(v["post"], json!(["ns:bbs", "ns:lab"]));
    assert_eq!(v["revision"], 2);
    let v = rig
        .run(
            &rig.scout,
            "grant",
            json!({"room": "ops", "manage": ["entity:will/ranger@bbs"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        v["managers"],
        json!(["entity:will/scout@bbs", "entity:will/ranger@bbs"])
    );
    let v = rig
        .run(
            &rig.scout,
            "revoke",
            json!({"room": "ops", "post": ["ns:lab"]}),
        )
        .await
        .unwrap();
    assert_eq!(v["post"], json!(["ns:bbs"]));
    let v = rig
        .run(&rig.scout, "archive", json!({"room": "ops"}))
        .await
        .unwrap();
    assert_eq!(v["archived"], true);
    let v = rig
        .run(&rig.scout, "archive", json!({"room": "ops"}))
        .await
        .unwrap();
    assert_eq!(v["changed"], false);
    let v = rig
        .run(
            &rig.scout,
            "describe",
            json!({"room": "ops", "text": "deploys"}),
        )
        .await
        .unwrap();
    assert_eq!(v["description"], "deploys");
    let v = rig
        .run(
            &rig.scout,
            "policy",
            json!({"room": "ops", "post": ["none"]}),
        )
        .await
        .unwrap();
    assert_eq!(v["post"], json!([]));
}

#[tokio::test]
async fn a_follow_starts_at_the_head_and_imports_each_page_once() {
    let rig = Rig::new();
    rig.host().create("feed", scout_entity());
    let at = rig.now();
    rig.host()
        .post("rooms/feed", "oncall@hub", "before the follow", at);
    let v = rig
        .run(&rig.ranger, "follow", json!({"room": "feed"}))
        .await
        .unwrap();
    assert_eq!(v["state"], "pending");
    rig.poll().await;
    let follows = rig.run(&rig.ranger, "follows", json!({})).await.unwrap();
    assert_eq!(follows["follows"][0]["state"], "following");
    let start: (Vec<u8>, i64) = rig
        .home
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT fetch_cursor, fetch_position FROM rooms WHERE name = 'feed'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let at = rig.now();
    let p1 = rig.host().post("rooms/feed", "oncall@hub", "one", at);
    rig.host()
        .post("rooms/feed", "lab/quiet-pine@lab", "two", at);
    rig.advance(61);
    rig.poll().await;
    // The same page again (as after a crash before the cursor moved)
    // queues nothing new.
    rig.home
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE rooms SET fetch_cursor = ?1, fetch_position = ?2 WHERE name = 'feed'",
            rusqlite::params![start.0, start.1],
        )
        .unwrap();
    rig.advance(61);
    rig.poll().await;
    let n: i64 = rig
        .home
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM room_deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    let msgs = rig.read(&rig.ranger);
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert_eq!(msgs[0].body, "one");
    assert_eq!(msgs[0].room.as_deref(), Some("rooms/feed@hub"));
    assert_eq!(msgs[0].reported, Some(true));
    assert_eq!(
        msgs[0].group_copy_sender().unwrap(),
        "id:oncall@hub via room rooms/feed@hub (reported)"
    );
    assert_eq!(
        msgs[1].group_copy_sender().unwrap(),
        "id:lab/quiet-pine@lab via room rooms/feed@hub (reported)"
    );
    // A private reply goes to the author, answering the post.
    let v = rig
        .run(
            &rig.ranger,
            "reply",
            json!({"room": "feed", "message": msgs[0].id.to_string(), "text": "dm", "private": true}),
        )
        .await
        .unwrap();
    assert_eq!(v["to"], "oncall@hub");
    assert_eq!(v["in_reply_to"], ref_display(&p1));
    assert!(rig.read(&rig.ranger).is_empty());
}

#[tokio::test]
async fn quiet_follows_wait_for_a_prompt_and_wake_follows_coalesce() {
    let rig = Rig::new();
    rig.host().create("a", scout_entity());
    rig.run(&rig.ranger, "follow", json!({"room": "a"}))
        .await
        .unwrap();
    rig.run(&rig.scout, "follow", json!({"room": "a", "wake": true}))
        .await
        .unwrap();
    rig.poll().await;
    let at = rig.now();
    rig.host().post("rooms/a", "oncall@hub", "first", at);
    rig.advance(16);
    rig.poll().await;
    // Quiet: nothing wakes a waiting reader; an explicit read shows it.
    assert!(rig.read_waiting(&rig.ranger).is_empty());
    assert_eq!(rig.read(&rig.ranger).len(), 1);
    // Wake: delivered at once.
    assert_eq!(rig.read_waiting(&rig.scout).len(), 1);
    // A second post within the minute waits for the next wake.
    let at = rig.now();
    rig.host().post("rooms/a", "oncall@hub", "second", at);
    rig.advance(16);
    rig.poll().await;
    assert!(rig.read_waiting(&rig.scout).is_empty());
    rig.advance(45);
    rig.poll().await;
    let woken = rig.read_waiting(&rig.scout);
    assert_eq!(woken.len(), 1);
    assert_eq!(woken[0].body, "second");
}

#[tokio::test]
async fn two_agents_in_one_room_see_each_others_posts() {
    let rig = Rig::new();
    rig.run(&rig.scout, "create", json!({"name": "war"}))
        .await
        .unwrap();
    for who in [&rig.scout, &rig.ranger] {
        rig.run(who, "follow", json!({"room": "war", "wake": true}))
            .await
            .unwrap();
    }
    rig.poll().await;
    let posted = rig
        .run(
            &rig.scout,
            "post",
            json!({"room": "war", "text": "I'm taking it"}),
        )
        .await
        .unwrap();
    assert_eq!(posted["state"], "queued");
    rig.run(&rig.ranger, "post", json!({"room": "war", "text": "ack"}))
        .await
        .unwrap();
    rig.hand_off(None);
    rig.poll().await;
    let seen_by_ranger: Vec<String> = rig
        .read(&rig.ranger)
        .into_iter()
        .map(|m| format!("{}: {}", m.original_from.unwrap_or_default(), m.body))
        .collect();
    assert_eq!(
        seen_by_ranger,
        [
            "id:will/scout@bbs: I'm taking it",
            "id:will/ranger@bbs: ack"
        ]
    );
    let seen_by_scout = rig.read(&rig.scout);
    assert!(seen_by_scout.iter().any(|m| m.body == "ack"));
    // The post confirmed by read-back (it reads at once after a post).
    let st = rig
        .run(
            &rig.scout,
            "status",
            json!({"message": posted["message"].to_string()}),
        )
        .await
        .unwrap();
    assert_eq!(st["state"], "confirmed");
    // The host's order decides who was first.
    let h = rig
        .run(&rig.ranger, "history", json!({"room": "war"}))
        .await
        .unwrap();
    assert_eq!(h["records"][0]["author"], "will/scout@bbs");
}

#[tokio::test]
async fn backfill_delivers_from_the_oldest_up_to_the_switch() {
    let rig = Rig::new();
    rig.host().create("b", scout_entity());
    let at = rig.now();
    for i in 0..7 {
        rig.host()
            .post("rooms/b", "oncall@hub", &format!("old {i}"), at);
    }
    rig.run(
        &rig.ranger,
        "follow",
        json!({"room": "b", "from": "oldest"}),
    )
    .await
    .unwrap();
    rig.poll().await; // starts at the head, with a backfill to it
    let at = rig.now();
    rig.host().post("rooms/b", "oncall@hub", "new", at);
    rig.settle().await;
    let bodies: Vec<String> = rig.read(&rig.ranger).into_iter().map(|m| m.body).collect();
    assert_eq!(bodies.len(), 8, "{bodies:?}");
    assert!(bodies.contains(&"old 0".to_string()));
    assert!(bodies.contains(&"new".to_string()));
    let follows = rig.run(&rig.ranger, "follows", json!({})).await.unwrap();
    assert_eq!(follows["follows"][0]["backfilling"], false);
}

#[tokio::test]
async fn restore_recovery_delivers_what_was_missed_once() {
    let rig = Rig::new();
    rig.host().create("r", scout_entity());
    rig.run(&rig.ranger, "follow", json!({"room": "r"}))
        .await
        .unwrap();
    rig.poll().await;
    let at = rig.now();
    rig.host().post("rooms/r", "oncall@hub", "kept", at);
    rig.host().post("rooms/r", "oncall@hub", "lost", at);
    rig.advance(61);
    rig.poll().await;
    assert_eq!(rig.read(&rig.ranger).len(), 2);
    // The host restores a backup with only the first post, then takes more.
    rig.host().restore(1);
    let at = rig.now();
    rig.host()
        .post("rooms/r", "oncall@hub", "after restore 1", at);
    rig.host()
        .post("rooms/r", "oncall@hub", "after restore 2", at);
    rig.advance(61);
    rig.poll().await; // cursor_invalid, then recovery
    let bodies: Vec<String> = rig.read(&rig.ranger).into_iter().map(|m| m.body).collect();
    assert_eq!(bodies, ["after restore 1", "after restore 2"]);
    // The next fetch reads after the noted head in the new epoch.
    let at = rig.now();
    rig.host().post("rooms/r", "oncall@hub", "later", at);
    rig.advance(61);
    rig.poll().await;
    let bodies: Vec<String> = rig.read(&rig.ranger).into_iter().map(|m| m.body).collect();
    assert_eq!(bodies, ["later"]);
    let epoch: i64 = rig
        .home
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT epoch FROM rooms WHERE name = 'r'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(epoch, 1);
}

#[tokio::test]
async fn at_a_prompt_an_agent_gets_the_newest_fifty_per_room() {
    let rig = Rig::new();
    rig.host().create("busy", scout_entity());
    rig.run(&rig.ranger, "follow", json!({"room": "busy"}))
        .await
        .unwrap();
    rig.poll().await;
    let at = rig.now();
    for i in 0..60 {
        rig.host()
            .post("rooms/busy", "oncall@hub", &format!("p{i}"), at);
    }
    rig.advance(61);
    rig.poll().await;
    let msgs = rig.read(&rig.ranger);
    assert_eq!(msgs.len(), 51);
    assert!(
        msgs[0].body.contains("10 earlier new posts"),
        "{}",
        msgs[0].body
    );
    assert_eq!(msgs[1].body, "p10");
    assert_eq!(msgs[50].body, "p59");
}

#[tokio::test]
async fn post_outcomes_confirm_reject_and_hold_tags() {
    let rig = Rig::new();
    rig.run(&rig.scout, "create", json!({"name": "o"}))
        .await
        .unwrap();
    let err = rig
        .run(
            &rig.scout,
            "post",
            json!({"room": "o", "text": "x", "tags": (0..9).map(|i| format!("a{i}@hub")).collect::<Vec<_>>()}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at most 8"), "{err:#}");
    let p = rig
        .run(
            &rig.scout,
            "post",
            json!({"room": "o", "text": "hold off", "tags": ["deployer@hub", "will/ranger@bbs"]}),
        )
        .await
        .unwrap();
    assert_eq!(p["state"], "queued");
    assert_eq!(p["tags"][0]["state"], "held");
    assert!(
        rig.outbox().iter().all(|m| m.mention.is_none()),
        "tags wait"
    );
    rig.hand_off(None);
    rig.poll().await;
    let st = rig
        .run(&rig.scout, "status", json!({"message": p["ref"]}))
        .await
        .unwrap();
    assert_eq!(st["state"], "confirmed");
    assert_eq!(st["tags"][0]["state"], "sent");
    let tag = rig
        .outbox()
        .into_iter()
        .find(|m| m.mention.is_some())
        .expect("a tag went out");
    assert_eq!(tag.recipient.unwrap().entity.unwrap().entity_id, "deployer");
    let mention = tag.mention.unwrap();
    assert_eq!(mention.room.unwrap().entity_id, "rooms/o");
    // The local tag is held, then merged into a verified delivery.
    rig.poll().await;
    let got = rig.read(&rig.ranger);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].body, "hold off");
    assert_eq!(got[0].room_tagged_by.as_deref(), Some("will/scout@bbs"));

    // Refused by the host: rejected, tags dropped, poster told.
    let p = rig
        .run(
            &rig.scout,
            "post",
            json!({"room": "o", "text": "no", "tags": ["x@hub"]}),
        )
        .await
        .unwrap();
    rig.hand_off(Some("sender_not_permitted"));
    rig.poll().await;
    let st = rig
        .run(
            &rig.scout,
            "status",
            json!({"message": p["message"].to_string()}),
        )
        .await
        .unwrap();
    assert_eq!(st["state"], "rejected");
    assert_eq!(st["reason"], "sender_not_permitted");
    assert_eq!(st["tags"][0]["state"], "dropped");
    let notices: Vec<String> = rig.read(&rig.scout).into_iter().map(|m| m.body).collect();
    assert!(
        notices
            .iter()
            .any(|b| b.contains("was rejected (sender_not_permitted)") && b.contains("x@hub")),
        "{notices:?}"
    );
}

#[tokio::test]
async fn a_post_absent_for_an_hour_of_answers_is_unconfirmed() {
    let rig = Rig::new();
    rig.run(&rig.scout, "create", json!({"name": "u"}))
        .await
        .unwrap();
    let p = rig
        .run(
            &rig.scout,
            "post",
            json!({"room": "u", "text": "lost", "tags": ["t@hub"]}),
        )
        .await
        .unwrap();
    // Taken by the first hop, but never reaches the room.
    {
        let mut conn = rig.home.conn.lock().unwrap();
        let (outbox, _) = integration::claim(&mut conn, "hub")
            .unwrap()
            .handoff
            .unwrap();
        integration::handed_off(&conn, outbox).unwrap();
    }
    rig.poll().await;
    let st = rig
        .run(&rig.scout, "status", json!({"message": p["ref"]}))
        .await
        .unwrap();
    assert_eq!(st["state"], "sent");
    // Unreachable time does not count.
    rig.host().down = true;
    for _ in 0..10 {
        rig.advance(900);
        rig.poll().await;
    }
    let st = rig
        .run(&rig.scout, "status", json!({"message": p["ref"]}))
        .await
        .unwrap();
    assert_eq!(st["state"], "sent");
    rig.host().down = false;
    for _ in 0..8 {
        rig.advance(900);
        rig.poll().await;
    }
    let st = rig
        .run(&rig.scout, "status", json!({"message": p["ref"]}))
        .await
        .unwrap();
    assert_eq!(st["state"], "unconfirmed");
    assert_eq!(st["tags"][0]["state"], "dropped");
}

#[tokio::test]
async fn a_tag_that_cannot_be_read_back_is_an_unverified_claim() {
    let rig = Rig::new();
    rig.host().create("t", scout_entity());
    // A tag from another home, for a post the room does not have.
    let at = rig.now();
    let m = mp::Message {
        id: Some(mp::MessageRef {
            namespace: "lab".into(),
            message_id: "tag1".into(),
        }),
        sender: Some(addr("lab", "lab/quiet-pine")),
        recipient: Some(addr("bbs", "will/ranger")),
        authored_at_unix_micros: at,
        expires_at_unix_micros: Some(at + 3_600_000_000),
        content_type: "text/plain; charset=utf-8".into(),
        body: b"look at this".to_vec(),
        kind: mp::MessageKind::Application as i32,
        mention: Some(mp::RoomMention {
            room: Some(mp::EntityRef {
                namespace: "hub".into(),
                entity_id: "rooms/t".into(),
            }),
            post: Some(mp::MessageRef {
                namespace: "lab".into(),
                message_id: "never".into(),
            }),
        }),
        ..Default::default()
    };
    let result = integration::import(
        &mut rig.home.conn.lock().unwrap(),
        "hub",
        &pb::MessageOffer {
            handoff_id: "h1".into(),
            message: Some(mp::EncodedMessage {
                format_major: 1,
                message_bytes: m.encode_to_vec(),
            }),
        },
    );
    assert_eq!(
        result.disposition,
        pb::HandoffDisposition::Received as i32,
        "{result:?}"
    );
    assert!(rig.read(&rig.ranger).is_empty(), "held until read back");
    // The import stamped the tag on the wall clock, ahead of the rig's.
    rig.advance(1);
    rig.poll().await;
    let got = rig.read(&rig.ranger);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "look at this");
    assert_eq!(got[0].room_unverified, Some(true));
    assert_eq!(got[0].room.as_deref(), Some("rooms/t@hub"));
}

#[tokio::test]
async fn a_room_the_host_does_not_have_ends_its_follows() {
    let rig = Rig::new();
    rig.run(&rig.ranger, "follow", json!({"room": "ghost"}))
        .await
        .unwrap();
    rig.poll().await;
    let msgs = rig.read(&rig.ranger);
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0]
            .body
            .contains("has ended: the host answered not_found")
    );
    let follows = rig.run(&rig.ranger, "follows", json!({})).await.unwrap();
    assert_eq!(follows["follows"], json!([]));
}

#[tokio::test]
async fn search_show_and_threads_read_the_room() {
    let rig = Rig::new();
    rig.host().create("s", scout_entity());
    let at = rig.now();
    let root = rig
        .host()
        .post("rooms/s", "oncall@hub", "gpu-07 is down", at);
    let reply = |host: &mut Host, parent: &mp::MessageRef, body: &str| {
        let (ns, path) = ("lab", "lab/quiet-pine");
        let id = mp::MessageRef {
            namespace: ns.into(),
            message_id: db::new_message_id(),
        };
        let m = mp::Message {
            id: Some(id.clone()),
            sender: Some(addr(ns, path)),
            recipient: Some(addr("hub", "rooms/s")),
            authored_at_unix_micros: at,
            content_type: "text/plain; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            in_reply_to: Some(parent.clone()),
            ..Default::default()
        };
        host.append("rooms/s", m, at);
        id
    };
    let r1 = reply(&mut rig.host(), &root, "on it");
    reply(&mut rig.host(), &r1, "thanks");
    let found = rig
        .run(&rig.scout, "search", json!({"room": "s", "text": "gpu-07"}))
        .await
        .unwrap();
    assert_eq!(found["records"].as_array().unwrap().len(), 1);
    let found = rig
        .run(
            &rig.scout,
            "search",
            json!({"room": "s", "author": "lab/quiet-pine@lab"}),
        )
        .await
        .unwrap();
    assert_eq!(found["records"].as_array().unwrap().len(), 2);
    let shown = rig
        .run(
            &rig.scout,
            "show",
            json!({"room": "s", "message": ref_display(&root), "thread": true}),
        )
        .await
        .unwrap();
    assert_eq!(shown["status"], "present");
    assert_eq!(shown["post"]["body"], "gpu-07 is down");
    let replies: Vec<(String, i64)> = shown["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["body"].as_str().unwrap().to_string(),
                r["depth"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        replies,
        [("on it".to_string(), 1), ("thanks".to_string(), 2)]
    );
}
