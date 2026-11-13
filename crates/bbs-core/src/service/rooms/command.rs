//! `bbs room <verb>`: every room action an agent takes, through its own
//! installation (requirement 8). A command's arguments and result are JSON
//! documents, so the CLI, the MCP tools and scripts share one shape.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use prost::Message as _;
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::super::{Caller, Ctx, check_focus, fmt_time, integration, send_direct, with_ctx_at};
use super::poll::{Answer, classify, found, request};
use super::store::{self, Found};
use super::{MAX_TAGS, RoomConfig, RoomHome, RoomName, address_display, b64, ref_display, unb64};
use crate::db;
use crate::hub_proto::msgproto_v1 as mp;

/// Every verb [`command`] takes.
pub const VERBS: &[&str] = &[
    "list",
    "info",
    "create",
    "describe",
    "policy",
    "grant",
    "revoke",
    "archive",
    "unarchive",
    "follow",
    "unfollow",
    "follows",
    "post",
    "reply",
    "status",
    "history",
    "search",
    "show",
];

/// A command's arguments; each verb reads the ones it takes.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Args {
    host: Option<String>,
    room: Option<String>,
    /// list: a substring of name or description.
    filter: Option<String>,
    /// list: include archived rooms.
    archived: bool,
    page_token: Option<String>,
    /// create: the room's name.
    name: Option<String>,
    /// create: its description.
    desc: Option<String>,
    /// create, policy: who may post; grant, revoke: one subject.
    post: Option<Vec<String>>,
    /// create: extra managers; grant, revoke: one subject.
    manage: Option<Vec<String>>,
    /// describe, post, reply: the text.
    text: Option<String>,
    /// follow: wake on new posts.
    wake: bool,
    /// follow: `oldest` or a cursor.
    from: Option<String>,
    /// post: the post it answers.
    reply: Option<String>,
    /// post: agents to tag, as PATH@NAMESPACE.
    tags: Vec<String>,
    /// post, reply: wait for the outcome.
    wait: bool,
    wait_secs: Option<u64>,
    /// reply, status, show: a message, as NAMESPACE:ID or this installation's
    /// message number.
    message: Option<String>,
    /// reply: privately, to the author.
    private: bool,
    limit: Option<u32>,
    before: Option<String>,
    author: Option<String>,
    since: Option<String>,
    until: Option<String>,
    thread: bool,
}

/// Who is acting: the caller, its native address and its entity.
#[derive(Debug, Clone)]
struct Me {
    principal: db::Principal,
    address: i64,
    entity: mp::EntityRef,
}

fn me(ctx: &Ctx) -> Result<Me> {
    let principal = ctx.actor()?;
    check_focus(ctx, &principal, None)?;
    let address = db::native_address_row(ctx.conn, principal.kind.into(), principal.id)?;
    let a = db::native_address(ctx.conn, principal.kind.into(), principal.id)?
        .context("the caller has no native address")?;
    Ok(Me {
        principal,
        address,
        entity: mp::EntityRef {
            namespace: a.entity.namespace,
            entity_id: a.entity.entity_id,
        },
    })
}

/// Run `f` as the caller in one transaction at the home's clock.
async fn tx<H, T, F>(home: &H, caller: &Caller, f: F) -> Result<T>
where
    H: RoomHome,
    T: Send + 'static,
    F: FnOnce(&Ctx) -> Result<T> + Send + 'static,
{
    let now = home.now();
    let caller = caller.clone();
    home.db(move |conn| with_ctx_at(conn, &caller, now, f))
        .await
}

/// Run room command `verb` for `caller` with JSON `args`.
///
/// Boxed, so the future is `Send` for every lifetime a server's RPC handler
/// needs (the compiler cannot prove it through the unboxed async chain).
pub fn command<'a, H: RoomHome>(
    home: &'a H,
    cfg: &'a RoomConfig,
    caller: &'a Caller,
    verb: &'a str,
    args: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
    Box::pin(run(home, cfg, caller, verb, args))
}

async fn run<H: RoomHome>(
    home: &H,
    cfg: &RoomConfig,
    caller: &Caller,
    verb: &str,
    args: &str,
) -> Result<Value> {
    let args: Args = if args.trim().is_empty() {
        Args::default()
    } else {
        serde_json::from_str(args).map_err(|e| {
            integration::InvalidRequest(format!("invalid arguments for room {verb}: {e}"))
        })?
    };
    let c = Cmd {
        home,
        cfg,
        caller,
        args,
    };
    match verb {
        "list" => c.list().await,
        "info" => c.info().await,
        "create" => c.create().await,
        "describe" | "policy" | "grant" | "revoke" | "archive" | "unarchive" => {
            c.update(verb).await
        }
        "follow" => c.follow().await,
        "unfollow" => c.unfollow().await,
        "follows" => c.follows().await,
        "post" => c.post(None).await,
        "reply" => c.reply().await,
        "status" => c.status().await,
        "history" => c.history().await,
        "search" => c.search().await,
        "show" => c.show().await,
        _ => Err(integration::InvalidRequest(format!(
            "unknown room verb {verb:?}; one of: {}",
            VERBS.join(", ")
        ))
        .into()),
    }
}

struct Cmd<'a, H> {
    home: &'a H,
    cfg: &'a RoomConfig,
    caller: &'a Caller,
    args: Args,
}

impl<H: RoomHome> Cmd<'_, H> {
    fn room(&self) -> Result<RoomName> {
        let room = self
            .args
            .room
            .as_deref()
            .context("this room command needs a room")?;
        RoomName::parse(room, self.args.host.as_deref(), &self.cfg.default_host)
    }

    fn host(&self) -> Result<String> {
        let host = self
            .args
            .host
            .clone()
            .unwrap_or_else(|| self.cfg.default_host.clone());
        db::validate_namespace(&host)?;
        Ok(host)
    }

    /// The caller and the integration that reaches `host`.
    async fn route(&self, host: &str) -> Result<(Me, String)> {
        let host = host.to_string();
        tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            let integration = store::integration_for(ctx, &host)?;
            Ok((me, integration))
        })
        .await
    }

    /// Ask `op` as the caller; an error for anything but an answer.
    async fn ask(
        &self,
        me: &Me,
        integration: &str,
        host: &str,
        room: &str,
        op: mp::room_request::Op,
    ) -> Answer {
        let req = request(me.entity.clone(), host, room, self.home.now(), op);
        classify(self.home.ask(integration.to_string(), req).await)
    }

    async fn ask_ok(
        &self,
        me: &Me,
        integration: &str,
        host: &str,
        room: &str,
        op: mp::room_request::Op,
    ) -> Result<mp::RoomAnswer> {
        answered(self.ask(me, integration, host, room, op).await, host)
    }

    async fn info_of(&self, me: &Me, integration: &str, room: &RoomName) -> Result<mp::RoomInfo> {
        let a = self
            .ask_ok(
                me,
                integration,
                &room.host,
                &room.path(),
                mp::room_request::Op::Info(true),
            )
            .await?;
        let Some(mp::room_answer::Result::Info(info)) = a.result else {
            bail!(
                "invalid_message: {} answered Info without a room",
                room.host
            );
        };
        let (name, archived) = (room.clone(), info.archived);
        tx(self.home, self.caller, move |ctx| {
            store::set_archived(ctx, &name, archived)
        })
        .await?;
        Ok(info)
    }

    async fn list(&self) -> Result<Value> {
        let host = self.host()?;
        let (me, integration) = self.route(&host).await?;
        let op = mp::room_request::Op::List(mp::RoomList {
            filter: self.args.filter.clone().unwrap_or_default(),
            include_archived: self.args.archived,
            page_size: self.args.limit.unwrap_or(50).min(200),
            page_token: self
                .args
                .page_token
                .as_deref()
                .map(unb64)
                .transpose()?
                .unwrap_or_default(),
        });
        let a = self.ask_ok(&me, &integration, &host, "", op).await?;
        let Some(mp::room_answer::Result::List(list)) = a.result else {
            bail!("invalid_message: {host} answered List without a list");
        };
        Ok(json!({
            "host": host,
            "rooms": list.rooms.iter().map(|i| info_json(&host, i)).collect::<Vec<_>>(),
            "next_page_token": opt_b64(&list.next_page_token),
        }))
    }

    async fn info(&self) -> Result<Value> {
        let room = self.room()?;
        let (me, integration) = self.route(&room.host).await?;
        let info = self.info_of(&me, &integration, &room).await?;
        Ok(info_json(&room.host, &info))
    }

    async fn create(&self) -> Result<Value> {
        let host = self.host()?;
        let name = self.args.name.clone().context("room create needs a NAME")?;
        let name = name.strip_prefix("rooms/").unwrap_or(&name).to_string();
        if !super::valid_name(&name) {
            bail!("invalid room name {name:?}: use [a-z0-9][a-z0-9-]{{0,62}}");
        }
        let room = RoomName {
            host: host.clone(),
            name: name.clone(),
        };
        let (me, integration) = self.route(&host).await?;
        let post = subjects(
            self.args.post.as_deref().unwrap_or(&["any".to_string()]),
            &me,
        )?;
        // BBS agents are persistent: no extra managers by default (D5).
        let managers = subjects(self.args.manage.as_deref().unwrap_or(&[]), &me)?;
        let mut create = mp::RoomCreate {
            operation_id: String::new(),
            name: name.clone(),
            description: self.args.desc.clone().unwrap_or_default(),
            post,
            managers,
        };
        let fingerprint = fingerprint(&host, "", &create.encode_to_vec());
        create.operation_id = self.operation(&me, &host, &fingerprint).await?;
        let op_id = create.operation_id.clone();
        let answer = self
            .ask(
                &me,
                &integration,
                &host,
                "",
                mp::room_request::Op::Create(create),
            )
            .await;
        self.answered_op(&answer, &op_id).await?;
        match answer {
            Answer::Ok(a) => {
                let Some(mp::room_answer::Result::Info(info)) = a.result else {
                    bail!("invalid_message: {host} answered Create without a room");
                };
                let mut v = info_json(&host, &info);
                v["created"] = json!(true);
                Ok(v)
            }
            // Exists, and the caller manages it: as good as created (D2).
            Answer::Refused(code, detail, a) if code == "conflict" => match a.result {
                Some(mp::room_answer::Result::Info(info)) if manages(&info, &me) => {
                    let mut v = info_json(&host, &info);
                    v["created"] = json!(false);
                    Ok(v)
                }
                _ => bail!(
                    "conflict: {} exists and you do not manage it{}",
                    room.display(),
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(" ({detail})")
                    }
                ),
            },
            other => answered(other, &host).map(|_| Value::Null),
        }
    }

    /// The operation ID for a change: the pending one for the same command,
    /// else a new one, saved before the request goes (D2).
    async fn operation(&self, me: &Me, host: &str, fingerprint: &str) -> Result<String> {
        let (address, host, fingerprint) = (me.address, host.to_string(), fingerprint.to_string());
        tx(self.home, self.caller, move |ctx| {
            let pending: Option<String> = ctx
                .conn
                .query_row(
                    "SELECT operation_id FROM room_operations WHERE requester_address_id = ?1
                     AND host = ?2 AND fingerprint = ?3 AND state = 'pending'",
                    params![address, host, fingerprint],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = pending {
                return Ok(id);
            }
            let id = db::new_message_id();
            ctx.conn.execute(
                "INSERT INTO room_operations (requester_address_id, host, fingerprint,
                     operation_id, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
                params![address, host, fingerprint, id, ctx.now_s],
            )?;
            Ok(id)
        })
        .await
    }

    /// A signed answer settles an operation: the next command gets a new ID.
    async fn answered_op(&self, answer: &Answer, op_id: &str) -> Result<()> {
        if matches!(answer, Answer::Unavailable(..)) {
            return Ok(());
        }
        let op_id = op_id.to_string();
        tx(self.home, self.caller, move |ctx| {
            ctx.conn.execute(
                "UPDATE room_operations SET state = 'answered', answered_at = ?2
                 WHERE operation_id = ?1",
                params![op_id, ctx.now_s],
            )?;
            Ok(())
        })
        .await
    }

    /// describe, policy, grant, revoke, archive, unarchive: Info, then an
    /// Update at its revision (retried once on a revision race).
    async fn update(&self, verb: &str) -> Result<Value> {
        let room = self.room()?;
        let (me, integration) = self.route(&room.host).await?;
        for attempt in 0..2 {
            let info = self.info_of(&me, &integration, &room).await?;
            let Some(mut update) = self.change(verb, &info, &me)? else {
                let mut v = info_json(&room.host, &info);
                v["changed"] = json!(false);
                return Ok(v);
            };
            update.expected_revision = info.revision;
            let fingerprint = fingerprint(&room.host, &room.path(), &update.encode_to_vec());
            update.operation_id = self.operation(&me, &room.host, &fingerprint).await?;
            let op_id = update.operation_id.clone();
            let answer = self
                .ask(
                    &me,
                    &integration,
                    &room.host,
                    &room.path(),
                    mp::room_request::Op::Update(update),
                )
                .await;
            self.answered_op(&answer, &op_id).await?;
            match answer {
                Answer::Refused(code, _, _) if code == "revision_mismatch" && attempt == 0 => {
                    continue;
                }
                other => {
                    let a = answered(other, &room.host)?;
                    let Some(mp::room_answer::Result::Info(info)) = a.result else {
                        bail!(
                            "invalid_message: {} answered Update without a room",
                            room.host
                        );
                    };
                    let (name, archived) = (room.clone(), info.archived);
                    tx(self.home, self.caller, move |ctx| {
                        store::set_archived(ctx, &name, archived)
                    })
                    .await?;
                    let mut v = info_json(&room.host, &info);
                    v["changed"] = json!(true);
                    return Ok(v);
                }
            }
        }
        bail!(
            "revision_mismatch: {} kept changing; try again",
            room.display()
        )
    }

    /// The Update a verb makes of `info`, or None when it changes nothing.
    fn change(&self, verb: &str, info: &mp::RoomInfo, me: &Me) -> Result<Option<mp::RoomUpdate>> {
        let mut u = mp::RoomUpdate::default();
        match verb {
            "describe" => {
                let text = self
                    .args
                    .text
                    .clone()
                    .context("room describe needs the text")?;
                if text == info.description {
                    return Ok(None);
                }
                u.description = Some(text);
            }
            "policy" => {
                let post = self
                    .args
                    .post
                    .as_deref()
                    .context("room policy needs --post SUBJECT,...")?;
                u.set_post = true;
                u.post = subjects(post, me)?;
            }
            "grant" | "revoke" => {
                let (list, managers) = match (&self.args.post, &self.args.manage) {
                    (Some(p), None) => (p, false),
                    (None, Some(m)) => (m, true),
                    _ => bail!("room {verb} takes exactly one of --post or --manage"),
                };
                let wanted = subjects(list, me)?;
                let current = if managers { &info.managers } else { &info.post };
                let mut next = current.clone();
                for s in wanted {
                    let has = next.contains(&s);
                    match (verb, has) {
                        ("grant", false) => next.push(s),
                        ("revoke", true) => next.retain(|x| *x != s),
                        _ => {}
                    }
                }
                if next == *current {
                    return Ok(None);
                }
                if managers {
                    u.set_managers = true;
                    u.managers = next;
                } else {
                    u.set_post = true;
                    u.post = next;
                }
            }
            "archive" | "unarchive" => {
                let archived = verb == "archive";
                if info.archived == archived {
                    return Ok(None);
                }
                u.archived = Some(archived);
            }
            _ => unreachable!(),
        }
        Ok(Some(u))
    }

    async fn follow(&self) -> Result<Value> {
        let room = self.room()?;
        let mode = if self.args.wake { "wake" } else { "quiet" };
        let (kind, cursor) = match self.args.from.as_deref() {
            None | Some("head") => ("head", None),
            Some("oldest") => ("oldest", None),
            Some(c) => ("cursor", Some(unb64(c)?)),
        };
        let name = room.clone();
        let (state, existed) = tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            let id = store::room_id(ctx, &name)?;
            ctx.conn.execute(
                "UPDATE rooms SET ended = NULL WHERE id = ?1 AND ended IS NOT NULL",
                [id],
            )?;
            let existing: Option<Option<i64>> = ctx
                .conn
                .query_row(
                    "SELECT start_position FROM room_follows WHERE room_id = ?1
                     AND follower_address_id = ?2",
                    params![id, me.address],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                Some(start) => {
                    ctx.conn.execute(
                        "UPDATE room_follows SET mode = ?3 WHERE room_id = ?1
                         AND follower_address_id = ?2",
                        params![id, me.address, mode],
                    )?;
                    Ok((
                        if start.is_some() {
                            "following"
                        } else {
                            "pending"
                        },
                        true,
                    ))
                }
                None => {
                    ctx.conn.execute(
                        "INSERT INTO room_follows (room_id, follower_address_id, mode, start_kind,
                             start_cursor, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![id, me.address, mode, kind, cursor, ctx.now_s],
                    )?;
                    store::fetch_now(ctx, id)?;
                    Ok(("pending", false))
                }
            }
        })
        .await?;
        self.home.changed();
        Ok(json!({
            "room": room.display(),
            "mode": mode,
            "state": state,
            "already_following": existed,
            "from": if existed { Value::Null } else { json!(kind) },
        }))
    }

    async fn unfollow(&self) -> Result<Value> {
        let room = self.room()?;
        let name = room.clone();
        let removed = tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            let Some(id): Option<i64> = ctx
                .conn
                .query_row(
                    "SELECT id FROM rooms WHERE host = ?1 AND name = ?2",
                    params![name.host, name.name],
                    |r| r.get(0),
                )
                .optional()?
            else {
                return Ok(false);
            };
            ctx.conn.execute(
                "DELETE FROM room_deliveries WHERE room_id = ?1 AND follower_address_id = ?2
                 AND state = 'pending' AND tag_message_id IS NULL",
                params![id, me.address],
            )?;
            Ok(ctx.conn.execute(
                "DELETE FROM room_follows WHERE room_id = ?1 AND follower_address_id = ?2",
                params![id, me.address],
            )? > 0)
        })
        .await?;
        Ok(json!({"room": room.display(), "unfollowed": removed}))
    }

    async fn follows(&self) -> Result<Value> {
        tx(self.home, self.caller, |ctx| {
            let me = me(ctx)?;
            let rows = ctx
                .conn
                .prepare(
                    "SELECT r.host, r.name, f.mode, f.start_kind, f.start_position IS NOT NULL,
                        f.backfill_until IS NOT NULL, r.archived, f.created_at,
                        (SELECT count(*) FROM room_deliveries d WHERE d.room_id = r.id
                           AND d.follower_address_id = f.follower_address_id
                           AND d.state = 'pending'),
                        (SELECT count(*) FROM room_deliveries d WHERE d.room_id = r.id
                           AND d.follower_address_id = f.follower_address_id
                           AND d.state = 'delivered')
                     FROM room_follows f JOIN rooms r ON r.id = f.room_id
                     WHERE f.follower_address_id = ?1 ORDER BY r.host, r.name",
                )?
                .query_map([me.address], |r| {
                    let room = RoomName {
                        host: r.get(0)?,
                        name: r.get(1)?,
                    };
                    let started: bool = r.get(4)?;
                    Ok(json!({
                        "room": room.display(),
                        "mode": r.get::<_, String>(2)?,
                        "from": r.get::<_, String>(3)?,
                        "state": if started { "following" } else { "pending" },
                        "backfilling": r.get::<_, bool>(5)?,
                        "archived": r.get::<_, bool>(6)?,
                        "since": r.get::<_, String>(7)?,
                        "undelivered": r.get::<_, i64>(8)?,
                        "delivered": r.get::<_, i64>(9)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "follows": rows }))
        })
        .await
    }

    /// Post to a room, optionally in reply to `reply`, with tags held until
    /// it is confirmed (D9, D10).
    async fn post(&self, reply: Option<mp::MessageRef>) -> Result<Value> {
        let room = self.room()?;
        let text = self.args.text.clone().context("room post needs the text")?;
        if text.trim().is_empty() {
            bail!("a post is empty");
        }
        if self.args.tags.len() > MAX_TAGS {
            bail!(
                "a post may tag at most {MAX_TAGS} agents; this one tags {}",
                self.args.tags.len()
            );
        }
        let mut tags = Vec::new();
        for t in &self.args.tags {
            let t = t.trim();
            crate::entity::parse_remote(t.strip_prefix("id:").unwrap_or(t)).with_context(|| {
                format!("invalid tag {t:?}: use a full address, PATH@NAMESPACE")
            })?;
            tags.push(t.to_string());
        }
        let reply_text = self.args.reply.clone();
        let name = room.clone();
        let message = tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            let reply = match (reply, reply_text) {
                (Some(r), _) => Some(r),
                (None, Some(t)) => Some(resolve_ref(ctx, &me, &t)?.0),
                (None, None) => None,
            };
            let id = store::room_id(ctx, &name)?;
            let reference = format!("{}@{}", name.path(), name.host);
            let (message, _) = integration::send_remote(
                ctx,
                &me.principal,
                &reference,
                &text,
                reply
                    .as_ref()
                    .map(|r| (r.namespace.as_str(), r.message_id.as_str())),
            )?;
            store::record_post(ctx, message, id, &tags)?;
            // A post reads its room at once (D7).
            store::fetch_now(ctx, id)?;
            Ok(message)
        })
        .await?;
        self.home.changed();
        if self.args.wait {
            return self.wait_outcome(message).await;
        }
        self.outcome_json(message).await
    }

    async fn outcome_json(&self, message: i64) -> Result<Value> {
        let o = self
            .home
            .db(move |conn| store::outcome(conn, message))
            .await?
            .context("no such room post")?;
        Ok(outcome_json(&o))
    }

    /// Wait on a post's outcome record until it settles or `wait_secs`.
    async fn wait_outcome(&self, message: i64) -> Result<Value> {
        let until = tokio::time::Instant::now()
            + Duration::from_secs(self.args.wait_secs.unwrap_or(60).min(600));
        loop {
            let o = self
                .home
                .db(move |conn| store::outcome(conn, message))
                .await?
                .context("no such room post")?;
            if o.settled() || o.state == "unconfirmed" || tokio::time::Instant::now() >= until {
                let mut v = outcome_json(&o);
                v["waited"] = json!(true);
                return Ok(v);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn reply(&self) -> Result<Value> {
        let text = self
            .args
            .message
            .clone()
            .context("room reply needs a message M")?;
        if !self.args.private {
            let r = self.ref_of(&text).await?;
            return self.post(Some(r)).await;
        }
        let body = self
            .args
            .text
            .clone()
            .context("room reply needs the text")?;
        if body.trim().is_empty() {
            bail!("a reply is empty");
        }
        let room = self.room()?;
        // The author as delivered here, else as stored, else read back.
        let (actor, r, author) = {
            let (t, name) = (text.clone(), room.clone());
            tx(self.home, self.caller, move |ctx| {
                let me = me(ctx)?;
                let (r, local) = resolve_ref(ctx, &me, &t)?;
                let author = author_of(ctx, &name, &r, local)?;
                Ok((me, r, author))
            })
            .await?
        };
        let author = match author {
            Some(a) => a,
            None => {
                let (_, integration) = self.route(&room.host).await?;
                let a = self
                    .ask_ok(&actor, &integration, &room.host, &room.path(), refs_op(&r))
                    .await?;
                let answer = Answer::Ok(a);
                match found(&answer, &r) {
                    Found::Present(rec) => {
                        let f = store::decode_record(rec).context("an unreadable room record")?;
                        f.message.sender.context("a post with no author")?
                    }
                    Found::Missing(why) => {
                        bail!("{} has no post {}: {why}", room.display(), ref_display(&r))
                    }
                    Found::Unanswered => {
                        bail!("no answer from {} about {}", room.host, ref_display(&r))
                    }
                }
            }
        };
        let to = address_display(&author);
        let (r2, author2) = (r.clone(), author.clone());
        let sent = tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            let e = author2.entity.clone().unwrap_or_default();
            let answers = Some((r2.namespace.as_str(), r2.message_id.as_str()));
            if e.namespace == db::namespace(ctx.conn)? {
                let found = db::local_entity(ctx.conn, &e.entity_id, &author2.address_id)?
                    .and_then(|f| f.principal)
                    .with_context(|| {
                        format!("no local agent or human {}", address_display(&author2))
                    })?;
                Ok(send_direct(ctx, &me.principal, found, &body, answers)?.id)
            } else {
                let reference = address_display(&author2);
                Ok(Some(
                    integration::send_remote(ctx, &me.principal, &reference, &body, answers)?.0,
                ))
            }
        })
        .await?;
        self.home.changed();
        Ok(json!({
            "private": true,
            "to": to,
            "in_reply_to": ref_display(&r),
            "message": sent,
            "note": "sent as a direct message; this reports only what this installation saw",
        }))
    }

    async fn ref_of(&self, text: &str) -> Result<mp::MessageRef> {
        let t = text.to_string();
        tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            Ok(resolve_ref(ctx, &me, &t)?.0)
        })
        .await
    }

    /// A post's outcome, read back at once when it is still open (D9).
    async fn status(&self) -> Result<Value> {
        let text = self
            .args
            .message
            .clone()
            .context("room status needs a message M")?;
        let message = tx(self.home, self.caller, move |ctx| {
            let me = me(ctx)?;
            own_post(ctx, &me, &text)
        })
        .await?;
        let o = self
            .home
            .db(move |conn| store::outcome(conn, message))
            .await?
            .context("no such room post")?;
        if !o.settled() && matches!(o.state, "sent" | "unknown" | "unconfirmed") {
            let (me, integration) = self.route(&o.room.host).await?;
            let answer = self
                .ask(
                    &me,
                    &integration,
                    &o.room.host,
                    &o.room.path(),
                    refs_op(&o.origin),
                )
                .await;
            let cfg = self.cfg.clone();
            let origin = o.origin.clone();
            let answer = std::sync::Arc::new(answer);
            tx(self.home, self.caller, move |ctx| {
                store::settle_readback(ctx, &cfg, message, found(&answer, &origin))
            })
            .await?;
            self.home.changed();
        }
        self.outcome_json(message).await
    }

    async fn history(&self) -> Result<Value> {
        let room = self.room()?;
        let (me, integration) = self.route(&room.host).await?;
        let (mode, cursor) = match &self.args.before {
            Some(c) => (mp::RoomReadMode::Before, unb64(c)?),
            None => (mp::RoomReadMode::Latest, Vec::new()),
        };
        let op = mp::room_request::Op::Read(mp::RoomRead {
            mode: mode as i32,
            cursor,
            limit: self.args.limit.unwrap_or(20).clamp(1, 200),
            ..Default::default()
        });
        let a = self
            .ask_ok(&me, &integration, &room.host, &room.path(), op)
            .await?;
        page_json(&room, a)
    }

    async fn search(&self) -> Result<Value> {
        let room = self.room()?;
        let (me, integration) = self.route(&room.host).await?;
        let now = self.home.now();
        let author = self.args.author.as_deref().map(entity_ref).transpose()?;
        let op = mp::room_request::Op::Read(mp::RoomRead {
            mode: match self.args.before {
                Some(_) => mp::RoomReadMode::Before,
                None => mp::RoomReadMode::Latest,
            } as i32,
            cursor: self
                .args
                .before
                .as_deref()
                .map(unb64)
                .transpose()?
                .unwrap_or_default(),
            text: self.args.text.clone().unwrap_or_default(),
            author,
            since_unix_micros: self
                .args
                .since
                .as_deref()
                .map(|s| when(s, now))
                .transpose()?,
            until_unix_micros: self
                .args
                .until
                .as_deref()
                .map(|s| when(s, now))
                .transpose()?,
            limit: self.args.limit.unwrap_or(50).clamp(1, 200),
            ..Default::default()
        });
        let a = self
            .ask_ok(&me, &integration, &room.host, &room.path(), op)
            .await?;
        page_json(&room, a)
    }

    /// One post by reference, and with `thread` its replies, walked level by
    /// level (D2: the host returns direct replies).
    async fn show(&self) -> Result<Value> {
        let room = self.room()?;
        let text = self
            .args
            .message
            .clone()
            .context("room show needs a message M")?;
        let r = self.ref_of(&text).await?;
        let (me, integration) = self.route(&room.host).await?;
        let a = self
            .ask_ok(&me, &integration, &room.host, &room.path(), refs_op(&r))
            .await?;
        let answer = Answer::Ok(a);
        let (status, post) = match found(&answer, &r) {
            Found::Present(rec) => ("present", Some(record_json(&room, rec))),
            Found::Missing(why) if why == "absent" => ("absent", None),
            Found::Missing(why) => {
                let mut v = json!({"room": room.display(), "ref": ref_display(&r),
                    "status": "refused", "reason": why});
                v["replies"] = json!([]);
                return Ok(v);
            }
            Found::Unanswered => bail!("no answer from {} about {}", room.host, ref_display(&r)),
        };
        let mut replies = Vec::new();
        if self.args.thread && post.is_some() {
            let mut level = vec![r.clone()];
            for depth in 1..=4 {
                let mut next = Vec::new();
                for parent in &level {
                    let op = mp::room_request::Op::Read(mp::RoomRead {
                        mode: mp::RoomReadMode::Latest as i32,
                        in_reply_to: Some(parent.clone()),
                        limit: 100,
                        ..Default::default()
                    });
                    let a = self
                        .ask_ok(&me, &integration, &room.host, &room.path(), op)
                        .await?;
                    let Some(mp::room_answer::Result::Page(page)) = a.result else {
                        continue;
                    };
                    for rec in &page.records {
                        let mut v = record_json(&room, rec);
                        v["depth"] = json!(depth);
                        replies.push(v);
                        if let Some(id) = store::decode_record(rec).and_then(|f| f.message.id) {
                            next.push(id);
                        }
                    }
                }
                if next.is_empty() || replies.len() >= 200 {
                    break;
                }
                level = next;
            }
        }
        Ok(json!({
            "room": room.display(),
            "ref": ref_display(&r),
            "status": status,
            "post": post,
            "replies": replies,
        }))
    }
}

/// The answer, or the error the host or the way there gave.
fn answered(answer: Answer, host: &str) -> Result<mp::RoomAnswer> {
    match answer {
        Answer::Ok(a) => Ok(a),
        Answer::Refused(code, detail, _) => {
            if detail.is_empty() {
                bail!("{code}: refused by {host}")
            }
            bail!("{code}: {detail}")
        }
        Answer::Unavailable(code, detail) => {
            bail!(
                "{code}: no answer from {host}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            )
        }
    }
}

fn refs_op(r: &mp::MessageRef) -> mp::room_request::Op {
    mp::room_request::Op::Read(mp::RoomRead {
        mode: mp::RoomReadMode::Refs as i32,
        refs: vec![r.clone()],
        ..Default::default()
    })
}

fn fingerprint(host: &str, room: &str, op: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(host.as_bytes());
    h.update([0]);
    h.update(room.as_bytes());
    h.update([0]);
    h.update(op);
    data_encoding::HEXLOWER.encode(&h.finalize())
}

/// Whether the caller manages the room, as an agent or by namespace.
fn manages(info: &mp::RoomInfo, me: &Me) -> bool {
    info.managers.iter().any(|s| match &s.subject {
        Some(mp::room_subject::Subject::Entity(e)) => *e == me.entity,
        Some(mp::room_subject::Subject::Namespace(ns)) => *ns == me.entity.namespace,
        _ => false,
    })
}

/// Policy subjects from the CLI's forms (D3): `any`, `none`, `self`,
/// `ns:NAMESPACE`, `entity:PATH@NAMESPACE`. Entity paths go as entity IDs:
/// Hub's entity IDs are paths.
fn subjects(list: &[String], me: &Me) -> Result<Vec<mp::RoomSubject>> {
    use mp::room_subject::Subject as S;
    let mut out = Vec::new();
    let items: Vec<&str> = list
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for s in &items {
        let subject = match *s {
            "none" if items.len() == 1 => return Ok(Vec::new()),
            "none" => bail!("`none` stands alone: it means nobody but managers"),
            "any" => S::Any(true),
            "self" => S::Entity(me.entity.clone()),
            s if s.starts_with("ns:") => {
                let ns = &s[3..];
                db::validate_namespace(ns)?;
                S::Namespace(ns.to_string())
            }
            s => S::Entity(
                entity_ref(s.strip_prefix("entity:").unwrap_or(s)).with_context(|| {
                    format!(
                        "invalid subject {s:?}: use any, none, self, ns:NAMESPACE or \
                         entity:PATH@NAMESPACE"
                    )
                })?,
            ),
        };
        let subject = mp::RoomSubject {
            subject: Some(subject),
        };
        if !out.contains(&subject) {
            out.push(subject);
        }
    }
    Ok(out)
}

/// `PATH@NAMESPACE` as an entity.
fn entity_ref(s: &str) -> Result<mp::EntityRef> {
    let s = s.strip_prefix("id:").unwrap_or(s);
    let (path, ns) = s
        .rsplit_once('@')
        .with_context(|| format!("invalid agent {s:?}: expected PATH@NAMESPACE"))?;
    if !crate::entity::valid_entity_id(path) {
        bail!("invalid agent path {path:?}");
    }
    db::validate_namespace(ns)?;
    Ok(mp::EntityRef {
        namespace: ns.to_string(),
        entity_id: path.to_string(),
    })
}

/// A time filter: a duration back from now (`1h`) or RFC3339.
fn when(s: &str, now: DateTime<Utc>) -> Result<i64> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_micros());
    }
    let d = humantime::parse_duration(s)
        .with_context(|| format!("invalid time {s:?}: use a duration like 1h or RFC3339"))?;
    Ok((now - chrono::Duration::from_std(d)?).timestamp_micros())
}

/// A message reference from `NAMESPACE:ID`, or this installation's message
/// number: a room post delivered to the caller (its original) or one it sent.
fn resolve_ref(ctx: &Ctx, me: &Me, text: &str) -> Result<(mp::MessageRef, Option<i64>)> {
    let text = text.trim();
    if let Ok(id) = text.parse::<i64>() {
        let row: Option<(String, String, Option<String>, Option<String>, bool, bool)> = ctx
            .conn
            .query_row(
                "SELECT m.origin_namespace, m.origin_message_id, m.group_copy_namespace,
                        m.group_copy_message_id, m.sender_address_id = ?2,
                        EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.id
                          AND d.recipient_address_id = ?2)
                 FROM messages m WHERE m.id = ?1",
                params![id, me.address],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        return match row {
            Some((_, _, Some(ns), Some(mid), _, true)) => Ok((
                mp::MessageRef {
                    namespace: ns,
                    message_id: mid,
                },
                Some(id),
            )),
            Some((ns, mid, _, _, true, _)) => Ok((
                mp::MessageRef {
                    namespace: ns,
                    message_id: mid,
                },
                Some(id),
            )),
            _ => bail!("message {id} is neither a room post delivered to you nor one you sent"),
        };
    }
    let (ns, id) = text.split_once(':').with_context(|| {
        format!("invalid message {text:?}: use NAMESPACE:ID or a message number")
    })?;
    db::validate_namespace(ns)?;
    if !crate::entity::valid_id(id) {
        bail!("invalid message ID {id:?}");
    }
    Ok((
        mp::MessageRef {
            namespace: ns.to_string(),
            message_id: id.to_string(),
        },
        None,
    ))
}

/// The author of post `r` as this installation has it, if it does.
fn author_of(
    ctx: &Ctx,
    room: &RoomName,
    r: &mp::MessageRef,
    local: Option<i64>,
) -> Result<Option<mp::AddressRef>> {
    let _ = local;
    Ok(ctx
        .conn
        .query_row(
            "SELECT p.author_namespace, p.author_entity_id, p.author_address_id, p.author_run_id
             FROM room_posts p JOIN rooms r ON r.id = p.room_id
             WHERE r.host = ?1 AND r.name = ?2 AND p.ref_namespace = ?3
               AND p.ref_message_id = ?4",
            params![room.host, room.name, r.namespace, r.message_id],
            |row| {
                Ok(mp::AddressRef {
                    entity: Some(mp::EntityRef {
                        namespace: row.get(0)?,
                        entity_id: row.get(1)?,
                    }),
                    address_id: row.get(2)?,
                    run_id: Some(row.get::<_, String>(3)?).filter(|s| !s.is_empty()),
                })
            },
        )
        .optional()?)
}

/// The caller's own room post named by `text` (a number or `NS:ID`).
fn own_post(ctx: &Ctx, me: &Me, text: &str) -> Result<i64> {
    let text = text.trim();
    let id: Option<i64> = if let Ok(id) = text.parse::<i64>() {
        Some(id)
    } else {
        let (ns, mid) = text
            .split_once(':')
            .with_context(|| format!("invalid message {text:?}: use NAMESPACE:ID or a number"))?;
        ctx.conn
            .query_row(
                "SELECT id FROM messages WHERE origin_namespace = ?1 AND origin_message_id = ?2",
                params![ns, mid],
                |r| r.get(0),
            )
            .optional()?
    };
    let found: Option<i64> = match id {
        Some(id) => ctx
            .conn
            .query_row(
                "SELECT o.message_id FROM room_post_outcomes o JOIN messages m ON m.id = o.message_id
                 WHERE o.message_id = ?1 AND m.sender_address_id = ?2",
                params![id, me.address],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    found.with_context(|| format!("{text} is not a room post you sent"))
}

fn opt_b64(b: &[u8]) -> Value {
    if b.is_empty() {
        Value::Null
    } else {
        json!(b64(b))
    }
}

fn micros(t: i64) -> Value {
    DateTime::from_timestamp_micros(t).map_or(Value::Null, |t| json!(fmt_time(t)))
}

fn subject_json(s: &mp::RoomSubject) -> String {
    use mp::room_subject::Subject as S;
    match &s.subject {
        Some(S::Any(_)) => "any".to_string(),
        Some(S::Namespace(ns)) => format!("ns:{ns}"),
        Some(S::Entity(e)) => format!("entity:{}@{}", e.entity_id, e.namespace),
        None => "unknown".to_string(),
    }
}

fn info_json(host: &str, i: &mp::RoomInfo) -> Value {
    let name = i.name.strip_prefix("rooms/").unwrap_or(&i.name);
    json!({
        "room": format!("rooms/{name}@{host}"),
        "name": name,
        "host": host,
        "description": i.description,
        "post": i.post.iter().map(subject_json).collect::<Vec<_>>(),
        "managers": i.managers.iter().map(subject_json).collect::<Vec<_>>(),
        "archived": i.archived,
        "revision": i.revision,
        "head_position": i.head_position,
        "head_cursor": opt_b64(&i.head_cursor),
        "creator": i.creator.as_ref().map(|e| format!("{}@{}", e.entity_id, e.namespace)),
        "created_at": micros(i.created_at_unix_micros),
        "recent_authors": i.recent_authors.iter().map(address_display).collect::<Vec<_>>(),
        "address": i.address.as_ref().map(address_display),
    })
}

fn record_json(room: &RoomName, rec: &mp::RoomRecord) -> Value {
    let Some(f) = store::decode_record(rec) else {
        return json!({"position": rec.position, "unreadable": true});
    };
    let m = &f.message;
    let author = m.sender.as_ref().map(address_display);
    let notice = m
        .sender
        .as_ref()
        .and_then(|s| s.entity.as_ref())
        .is_some_and(|e| e.namespace == room.host && e.entity_id == room.path());
    json!({
        "ref": m.id.as_ref().map(ref_display),
        "position": rec.position,
        "cursor": b64(&rec.cursor),
        "accepted_at": f.accepted_at,
        "author": author,
        "author_reported": true,
        "notice": notice,
        "in_reply_to": m.in_reply_to.as_ref().map(ref_display),
        "content_type": m.content_type,
        "body": String::from_utf8_lossy(&m.body),
    })
}

fn page_json(room: &RoomName, a: mp::RoomAnswer) -> Result<Value> {
    let Some(mp::room_answer::Result::Page(page)) = a.result else {
        bail!(
            "invalid_message: {} answered Read without a page",
            room.host
        );
    };
    Ok(json!({
        "room": room.display(),
        "records": page.records.iter().map(|r| record_json(room, r)).collect::<Vec<_>>(),
        "next_cursor": opt_b64(&page.next_cursor),
        "head_position": page.head_position,
        "head_cursor": opt_b64(&page.head_cursor),
        "scan_truncated": page.scan_truncated,
    }))
}

fn outcome_json(o: &store::Outcome) -> Value {
    json!({
        "message": o.message,
        "ref": ref_display(&o.origin),
        "room": o.room.display(),
        "state": o.state,
        "reason": o.reason,
        "position": o.position,
        "tags": o.tags.iter().map(|(t, s)| json!({"target": t, "state": s})).collect::<Vec<_>>(),
    })
}
