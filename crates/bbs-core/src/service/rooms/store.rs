//! Room state in the database, each change inside the caller's transaction
//! ([`Ctx`]): rooms, follows and their shared fetch, fetched posts,
//! per-follower deliveries and their delivery to inboxes, post outcomes,
//! tags and received mentions.

use anyhow::{Context as _, Result, bail};
use prost::Message as _;
use rusqlite::{Connection, OptionalExtension as _, params};

use super::super::integration::{self, route};
use super::super::{Ctx, fmt_time};
use super::{MAX_BACKFILL, MAX_PER_ROOM, RoomConfig, RoomName};
use crate::db::{self, integration as dbi};
use crate::entity::NATIVE_ADDRESS;
use crate::hub_proto::msgproto_v1 as mp;
use crate::proto;

/// A room's row.
#[derive(Debug, Clone)]
pub(super) struct Room {
    pub id: i64,
    pub name: RoomName,
    pub fetch_cursor: Option<Vec<u8>>,
    pub fetch_position: Option<i64>,
    pub epoch: i64,
}

/// The room `name`'s row, created on first use.
pub(super) fn room_id(ctx: &Ctx, name: &RoomName) -> Result<i64> {
    ctx.conn.execute(
        "INSERT INTO rooms (host, name, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(host, name) DO NOTHING",
        params![name.host, name.name, ctx.now_s],
    )?;
    Ok(ctx.conn.query_row(
        "SELECT id FROM rooms WHERE host = ?1 AND name = ?2",
        params![name.host, name.name],
        |r| r.get(0),
    )?)
}

/// The room with row `id`.
pub(super) fn room(conn: &Connection, id: i64) -> Result<Room> {
    Ok(conn.query_row(
        "SELECT id, host, name, fetch_cursor, fetch_position, epoch FROM rooms WHERE id = ?1",
        [id],
        |r| {
            Ok(Room {
                id: r.get(0)?,
                name: RoomName {
                    host: r.get(1)?,
                    name: r.get(2)?,
                },
                fetch_cursor: r.get(3)?,
                fetch_position: r.get(4)?,
                epoch: r.get(5)?,
            })
        },
    )?)
}

/// Record whether the host says a room is archived (from an Info).
pub(super) fn set_archived(ctx: &Ctx, name: &RoomName, archived: bool) -> Result<()> {
    ctx.conn.execute(
        "UPDATE rooms SET archived = ?3 WHERE host = ?1 AND name = ?2",
        params![name.host, name.name, archived],
    )?;
    Ok(())
}

/// The integration that reaches `host`, as remote sends choose it.
pub(super) fn integration_for(ctx: &Ctx, host: &str) -> Result<String> {
    route(ctx, host, "rooms")
}

/// The room's address row (`rooms/NAME@HOST`), a foreign entity of kind
/// `room`, reached through `integration`.
fn room_address(ctx: &Ctx, name: &RoomName, integration: &str) -> Result<i64> {
    dbi::bind_namespace(ctx.conn, &name.host, integration)?;
    let entity = dbi::foreign_entity(ctx.conn, &name.host, &name.path(), "room", &ctx.now_s)?;
    dbi::foreign_address(ctx.conn, entity, NATIVE_ADDRESS, integration)
}

/// The integration a room's host is bound to, else the one routing picks.
fn bound_integration(ctx: &Ctx, host: &str) -> Result<String> {
    match dbi::namespace_integration(ctx.conn, host)? {
        Some(i) => Ok(i),
        None => integration_for(ctx, host),
    }
}

/// An author's address row: a native address for this installation's own
/// entities, else a foreign one.
fn author_address(ctx: &Ctx, author: &mp::AddressRef, integration: &str) -> Result<i64> {
    let e = author.entity.as_ref().context("a post with no author")?;
    let local = db::namespace(ctx.conn)?;
    if e.namespace == local {
        let found: Option<i64> = ctx
            .conn
            .query_row(
                "SELECT a.id FROM entities e
                 JOIN entity_addresses a ON a.entity_id = e.id AND a.address_id = ?3
                 WHERE e.namespace = ?1 AND e.entity_id = ?2",
                params![local, e.entity_id, author.address_id],
                |r| r.get(0),
            )
            .optional()?;
        return found.with_context(|| {
            format!(
                "the host reports {}@{local} as an author, which this installation does not have",
                e.entity_id
            )
        });
    }
    if dbi::namespace_integration(ctx.conn, &e.namespace)?.is_none() {
        dbi::bind_namespace(ctx.conn, &e.namespace, integration)?;
    }
    let entity = dbi::foreign_entity(ctx.conn, &e.namespace, &e.entity_id, "unknown", &ctx.now_s)?;
    let integration = dbi::namespace_integration(ctx.conn, &e.namespace)?
        .unwrap_or_else(|| integration.to_string());
    dbi::foreign_address(ctx.conn, entity, &author.address_id, &integration)
}

/// A post as fetched: the decoded message and its log metadata.
pub(super) struct Fetched {
    pub message: mp::Message,
    pub position: i64,
    pub cursor: Vec<u8>,
    pub accepted_at: String,
}

/// Decode a room record; None for one this installation cannot read.
pub(super) fn decode_record(rec: &mp::RoomRecord) -> Option<Fetched> {
    let encoded = rec.message.as_ref()?;
    if encoded.format_major != 1 {
        return None;
    }
    let message = mp::Message::decode(encoded.message_bytes.as_slice()).ok()?;
    message.id.as_ref()?;
    message.sender.as_ref()?.entity.as_ref()?;
    Some(Fetched {
        message,
        position: i64::try_from(rec.position).ok()?,
        cursor: rec.cursor.clone(),
        accepted_at: fmt_time(
            chrono::DateTime::from_timestamp_micros(rec.accepted_at_unix_micros)
                .unwrap_or_default(),
        ),
    })
}

/// Store a fetched post once per room; returns its row and whether it was
/// new. A post seen again (after a restore) keeps its row and moves to its
/// new position and epoch.
pub(super) fn upsert_post(ctx: &Ctx, room: &Room, f: &Fetched) -> Result<(i64, bool)> {
    let m = &f.message;
    let id = m.id.as_ref().unwrap();
    let existing: Option<i64> = ctx
        .conn
        .query_row(
            "SELECT id FROM room_posts WHERE room_id = ?1 AND ref_namespace = ?2
             AND ref_message_id = ?3",
            params![room.id, id.namespace, id.message_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(post) = existing {
        ctx.conn.execute(
            "UPDATE room_posts SET position = ?2, epoch = ?3, cursor = ?4 WHERE id = ?1",
            params![post, f.position, room.epoch, f.cursor],
        )?;
        return Ok((post, false));
    }
    let sender = m.sender.as_ref().unwrap();
    let author = sender.entity.as_ref().unwrap();
    let notice = author.namespace == room.name.host && author.entity_id == room.name.path();
    let body = String::from_utf8_lossy(&m.body).into_owned();
    let authored = fmt_time(
        chrono::DateTime::from_timestamp_micros(m.authored_at_unix_micros).unwrap_or_default(),
    );
    let (reply_ns, reply_id) = m
        .in_reply_to
        .as_ref()
        .map(|r| (r.namespace.clone(), r.message_id.clone()))
        .unzip();
    ctx.conn.execute(
        "INSERT INTO room_posts (room_id, ref_namespace, ref_message_id, position, epoch, cursor,
             accepted_at, authored_at, author_namespace, author_entity_id, author_address_id,
             author_run_id, in_reply_to_namespace, in_reply_to_message_id, content_type, body,
             notice)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            room.id,
            id.namespace,
            id.message_id,
            f.position,
            room.epoch,
            f.cursor,
            f.accepted_at,
            authored,
            author.namespace,
            author.entity_id,
            sender.address_id,
            sender.run_id.as_deref().unwrap_or(""),
            reply_ns,
            reply_id,
            m.content_type,
            body,
            notice,
        ],
    )?;
    Ok((ctx.conn.last_insert_rowid(), true))
}

/// Queue post `post` for `follower`, once per follower; returns whether it
/// is new to the follower.
pub(super) fn queue_delivery(
    ctx: &Ctx,
    room: i64,
    follower: i64,
    post: i64,
    source: &str,
    wake: bool,
) -> Result<bool> {
    let n = ctx.conn.execute(
        "INSERT INTO room_deliveries (room_id, follower_address_id, post_id, source, wake, state,
             created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)
         ON CONFLICT(follower_address_id, post_id) DO NOTHING",
        params![room, follower, post, source, wake, ctx.now_s],
    )?;
    Ok(n == 1)
}

/// A follow, as the fetch needs it.
#[derive(Debug, Clone)]
pub(super) struct Follow {
    pub follower: i64,
    pub wake: bool,
    pub start_position: Option<i64>,
    pub epoch: Option<i64>,
}

pub(super) fn follows(conn: &Connection, room: i64) -> Result<Vec<Follow>> {
    Ok(conn
        .prepare(
            "SELECT follower_address_id, mode = 'wake', start_position, epoch
             FROM room_follows WHERE room_id = ?1 ORDER BY id",
        )?
        .query_map([room], |r| {
            Ok(Follow {
                follower: r.get(0)?,
                wake: r.get(1)?,
                start_position: r.get(2)?,
                epoch: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// When to read a room next: its idle, archive and wake state, or its
/// backoff after reads with no answer (D7).
pub(super) fn schedule_fetch(ctx: &Ctx, cfg: &RoomConfig, room: i64) -> Result<()> {
    let (archived, last_post, failures, wake): (bool, Option<String>, i64, bool) =
        ctx.conn.query_row(
            "SELECT archived, last_post_at, failures,
                    EXISTS (SELECT 1 FROM room_follows WHERE room_id = r.id AND mode = 'wake')
             FROM rooms r WHERE id = ?1",
            [room],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let t = &cfg.timers;
    let base = if wake { t.poll_wake } else { t.poll_quiet };
    let idle_since = fmt_time(ctx.now - cfg.d(t.idle_after));
    let secs = if failures > 0 {
        (base * 2f64.powi(failures.min(10) as i32)).min(t.backoff_max)
    } else if archived {
        t.poll_archived
    } else if last_post.is_some_and(|t| t < idle_since) {
        t.poll_idle
    } else {
        base
    };
    let next = fmt_time(ctx.now + cfg.jittered(secs));
    ctx.conn.execute(
        "UPDATE rooms SET next_fetch_at = ?2 WHERE id = ?1",
        params![room, next],
    )?;
    Ok(())
}

/// Read a room at once (after a local post, a tag, or a new follow).
pub(super) fn fetch_now(ctx: &Ctx, room: i64) -> Result<()> {
    ctx.conn.execute(
        "UPDATE rooms SET next_fetch_at = ?2, kicks = kicks + 1 WHERE id = ?1",
        params![room, ctx.now_s],
    )?;
    Ok(())
}

/// The room's read-at-once count, taken before a read.
pub(super) fn kicks(conn: &Connection, room: i64) -> Result<i64> {
    Ok(
        conn.query_row("SELECT kicks FROM rooms WHERE id = ?1", [room], |r| {
            r.get(0)
        })?,
    )
}

/// After a read scheduled the next: a request to read at once that came
/// during it (`seen` is the count before it) still stands.
pub(super) fn keep_kick(ctx: &Ctx, room: i64, seen: i64) -> Result<()> {
    ctx.conn.execute(
        "UPDATE rooms SET next_fetch_at = ?3 WHERE id = ?1 AND kicks != ?2",
        params![room, seen, ctx.now_s],
    )?;
    Ok(())
}

/// The first read after a follow: the head, which starts every pending
/// follow (D7). `page` is a LATEST read.
pub(super) fn start_fetch(
    ctx: &Ctx,
    cfg: &RoomConfig,
    room: &Room,
    page: &mp::RoomPage,
) -> Result<()> {
    let head = i64::try_from(page.head_position).unwrap_or(i64::MAX);
    ctx.conn.execute(
        "UPDATE rooms SET fetch_cursor = ?2, fetch_position = ?3, failures = 0,
             last_post_at = coalesce(last_post_at, ?4)
         WHERE id = ?1",
        params![room.id, page.head_cursor, head, ctx.now_s],
    )?;
    let room = super::store::room(ctx.conn, room.id)?;
    join_pending(ctx, &room, head)?;
    schedule_fetch(ctx, cfg, room.id)
}

/// Start the room's pending follows at fetch position `at`: a follow from
/// the head starts there; one from earlier also gets a backfill up to it.
fn join_pending(ctx: &Ctx, room: &Room, at: i64) -> Result<()> {
    ctx.conn.execute(
        "UPDATE room_follows SET start_position = ?2, epoch = ?3,
             backfill_cursor = CASE WHEN start_kind = 'head' THEN NULL
                 ELSE coalesce(start_cursor, x'') END,
             backfill_until = CASE WHEN start_kind = 'head' THEN NULL ELSE ?2 END
         WHERE room_id = ?1 AND start_position IS NULL",
        params![room.id, at, room.epoch],
    )?;
    Ok(())
}

/// One fetched AFTER page, in one transaction (D7): store the posts, queue
/// each for every follower started before it, start pending follows, and
/// advance the fetch cursor. A crash repeats the read; deliveries are
/// deduplicated per follower. Returns how many deliveries it queued.
pub(super) fn apply_page(
    ctx: &Ctx,
    cfg: &RoomConfig,
    room: &Room,
    page: &mp::RoomPage,
) -> Result<usize> {
    let before = room.fetch_position.unwrap_or(0);
    // Follows from earlier than the head join at the position before this
    // page, so its posts reach them through the shared fetch.
    ctx.conn.execute(
        "UPDATE room_follows SET start_position = ?2, epoch = ?3,
             backfill_cursor = coalesce(start_cursor, x''), backfill_until = ?2
         WHERE room_id = ?1 AND start_position IS NULL AND start_kind != 'head'",
        params![room.id, before, room.epoch],
    )?;
    let started: Vec<Follow> = follows(ctx.conn, room.id)?
        .into_iter()
        .filter(|f| f.start_position.is_some() && f.epoch == Some(room.epoch))
        .collect();
    let mut queued = 0;
    let mut position = room.fetch_position;
    let mut last_post = None;
    for rec in &page.records {
        let Some(f) = decode_record(rec) else {
            tracing::warn!(
                room = room.name.display(),
                "skipping an unreadable room record"
            );
            continue;
        };
        if position.is_some_and(|p| f.position <= p) {
            continue;
        }
        position = Some(f.position);
        last_post = Some(f.accepted_at.clone());
        let (post, _) = upsert_post(ctx, room, &f)?;
        for follow in &started {
            if f.position > follow.start_position.unwrap()
                && queue_delivery(ctx, room.id, follow.follower, post, "follow", follow.wake)?
            {
                queued += 1;
            }
        }
    }
    let cursor = if page.next_cursor.is_empty() {
        room.fetch_cursor.clone().unwrap_or_default()
    } else {
        page.next_cursor.clone()
    };
    ctx.conn.execute(
        "UPDATE rooms SET fetch_cursor = ?2, fetch_position = ?3, failures = 0,
             last_post_at = coalesce(?4, last_post_at)
         WHERE id = ?1",
        params![room.id, cursor, position, last_post],
    )?;
    // Follows from the head start at the head this read saw.
    let head = i64::try_from(page.head_position).unwrap_or(i64::MAX);
    ctx.conn.execute(
        "UPDATE room_follows SET start_position = ?2, epoch = ?3
         WHERE room_id = ?1 AND start_position IS NULL AND start_kind = 'head'",
        params![room.id, head.max(position.unwrap_or(0)), room.epoch],
    )?;
    schedule_fetch(ctx, cfg, room.id)?;
    Ok(queued)
}

/// A read that got no answer: the cursor stays, and the room backs off.
pub(super) fn fetch_unanswered(ctx: &Ctx, cfg: &RoomConfig, room: i64) -> Result<()> {
    ctx.conn.execute(
        "UPDATE rooms SET failures = failures + 1 WHERE id = ?1",
        [room],
    )?;
    schedule_fetch(ctx, cfg, room)
}

/// The host refused the room for good (not_found, not_a_room, forbidden):
/// every follow ends, and each follower is told once (D7).
pub(super) fn end_follows(ctx: &Ctx, room: &Room, code: &str, detail: &str) -> Result<usize> {
    let follows = follows(ctx.conn, room.id)?;
    for f in &follows {
        room_notice(
            ctx,
            room,
            f.follower,
            &format!(
                "Your follow of {} has ended: the host answered {code}{}{}.",
                room.name.display(),
                if detail.is_empty() { "" } else { ": " },
                detail
            ),
        )?;
    }
    ctx.conn.execute(
        "DELETE FROM room_deliveries WHERE room_id = ?1 AND state = 'pending'",
        [room.id],
    )?;
    ctx.conn
        .execute("DELETE FROM room_follows WHERE room_id = ?1", [room.id])?;
    ctx.conn.execute(
        "UPDATE rooms SET ended = ?2, next_fetch_at = NULL WHERE id = ?1",
        params![room.id, code],
    )?;
    Ok(follows.len())
}

/// Restore recovery (D7, D8), in one transaction: the host's log was
/// restored or replaced, so the room's epoch moves on. `records` were read
/// backward from the head noted when recovery began (`head`, `head_cursor`),
/// down to a post already fetched or the bound (`bounded`). Posts not seen
/// before go to every started follow; every follow then starts at the noted
/// head in the new epoch, as the fetch does. Backfills in progress end.
pub(super) fn recover(
    ctx: &Ctx,
    cfg: &RoomConfig,
    room: &Room,
    head: i64,
    head_cursor: &[u8],
    records: &[mp::RoomRecord],
    bounded: bool,
) -> Result<usize> {
    let started: Vec<Follow> = follows(ctx.conn, room.id)?
        .into_iter()
        .filter(|f| f.start_position.is_some())
        .collect();
    let epoch = room.epoch + 1;
    ctx.conn.execute(
        "UPDATE rooms SET epoch = ?2, fetch_cursor = ?3, fetch_position = ?4, failures = 0
         WHERE id = ?1",
        params![room.id, epoch, head_cursor, head],
    )?;
    let room = super::store::room(ctx.conn, room.id)?;
    let mut queued = 0;
    let mut fetched: Vec<Fetched> = records.iter().filter_map(decode_record).collect();
    fetched.sort_by_key(|f| f.position);
    for f in &fetched {
        let (post, new) = upsert_post(ctx, &room, f)?;
        if !new {
            continue;
        }
        for follow in &started {
            if queue_delivery(ctx, room.id, follow.follower, post, "recovery", follow.wake)? {
                queued += 1;
            }
        }
    }
    let backfilling: Vec<i64> = ctx
        .conn
        .prepare(
            "SELECT follower_address_id FROM room_follows
             WHERE room_id = ?1 AND backfill_until IS NOT NULL",
        )?
        .query_map([room.id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for follower in backfilling {
        room_notice(
            ctx,
            &room,
            follower,
            &format!(
                "The backfill of {} stopped: its host's log was restored. `bbs room history \
                 {}` reads earlier posts.",
                room.name.display(),
                room.name.display()
            ),
        )?;
    }
    ctx.conn.execute(
        "UPDATE room_follows SET start_position = ?2, epoch = ?3, backfill_cursor = NULL,
             backfill_until = NULL
         WHERE room_id = ?1 AND start_position IS NOT NULL",
        params![room.id, head, epoch],
    )?;
    if bounded {
        for f in &started {
            room_notice(
                ctx,
                &room,
                f.follower,
                &format!(
                    "The host of {} restored its log, and recovery read back {} posts without \
                     finding one already delivered: posts may have been missed. `bbs room \
                     history {}` reads them.",
                    room.name.display(),
                    super::MAX_RECOVERY,
                    room.name.display()
                ),
            )?;
        }
    }
    tracing::warn!(
        room = room.name.display(),
        epoch,
        queued,
        bounded,
        "recovered a room after its host's log was restored"
    );
    schedule_fetch(ctx, cfg, room.id)?;
    Ok(queued)
}

/// Whether a post with this reference is already stored for the room.
pub(super) fn known_post(conn: &Connection, room: i64, r: &mp::MessageRef) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM room_posts WHERE room_id = ?1 AND ref_namespace = ?2
             AND ref_message_id = ?3)",
        params![room, r.namespace, r.message_id],
        |r| r.get(0),
    )?)
}

/// A backfill due: the follow, its cursor and its bound.
pub(super) struct Backfill {
    pub follow: i64,
    pub room: i64,
    pub follower: i64,
    pub cursor: Vec<u8>,
    pub until: i64,
    pub count: i64,
}

pub(super) fn backfills(conn: &Connection) -> Result<Vec<Backfill>> {
    Ok(conn
        .prepare(
            "SELECT f.id, f.room_id, f.follower_address_id, f.backfill_cursor, f.backfill_until,
                    f.backfill_count
             FROM room_follows f JOIN rooms r ON r.id = f.room_id
             WHERE f.backfill_until IS NOT NULL AND f.epoch = r.epoch AND r.ended IS NULL",
        )?
        .query_map([], |r| {
            Ok(Backfill {
                follow: r.get(0)?,
                room: r.get(1)?,
                follower: r.get(2)?,
                cursor: r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                until: r.get(4)?,
                count: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// One backfill page (D7): posts up to the follow's bound go to it alone,
/// until the bound, an empty page, or [`MAX_BACKFILL`] posts.
pub(super) fn apply_backfill(ctx: &Ctx, b: &Backfill, page: &mp::RoomPage) -> Result<usize> {
    let room = room(ctx.conn, b.room)?;
    let mut count = b.count;
    let mut queued = 0;
    let mut done = page.records.is_empty();
    let mut stopped = false;
    for rec in &page.records {
        let Some(f) = decode_record(rec) else {
            continue;
        };
        if f.position > b.until {
            done = true;
            break;
        }
        if count >= MAX_BACKFILL {
            stopped = true;
            break;
        }
        let (post, _) = upsert_post(ctx, &room, &f)?;
        if queue_delivery(ctx, room.id, b.follower, post, "backfill", false)? {
            queued += 1;
        }
        count += 1;
    }
    if count >= MAX_BACKFILL && !done {
        stopped = true;
    }
    if page.next_cursor.is_empty() || page.next_cursor == b.cursor {
        done = true;
    }
    if stopped {
        room_notice(
            ctx,
            &room,
            b.follower,
            &format!(
                "The backfill of {} stopped at {MAX_BACKFILL} posts; later posts before your \
                 follow began were skipped. `bbs room history {} --before CURSOR` reads them \
                 (the backfill stopped at cursor {}).",
                room.name.display(),
                room.name.display(),
                super::b64(&page.next_cursor),
            ),
        )?;
    }
    if done || stopped {
        ctx.conn.execute(
            "UPDATE room_follows SET backfill_cursor = NULL, backfill_until = NULL,
                 backfill_count = ?2 WHERE id = ?1",
            params![b.follow, count],
        )?;
    } else {
        ctx.conn.execute(
            "UPDATE room_follows SET backfill_cursor = ?2, backfill_count = ?3 WHERE id = ?1",
            params![b.follow, page.next_cursor, count],
        )?;
    }
    Ok(queued)
}

/// End a backfill the host can no longer serve, telling its follower.
pub(super) fn end_backfill(ctx: &Ctx, b: &Backfill, why: &str) -> Result<()> {
    let room = room(ctx.conn, b.room)?;
    room_notice(
        ctx,
        &room,
        b.follower,
        &format!(
            "The backfill of {} stopped: {why}. `bbs room history {}` reads earlier posts.",
            room.name.display(),
            room.name.display()
        ),
    )?;
    ctx.conn.execute(
        "UPDATE room_follows SET backfill_cursor = NULL, backfill_until = NULL WHERE id = ?1",
        [b.follow],
    )?;
    Ok(())
}

// ---- delivery to inboxes -------------------------------------------------

/// A message from the room to `follower`, outside its log: an ended follow,
/// a stopped backfill, possible loss after a restore. It shows as the
/// room's notice.
pub(super) fn room_notice(ctx: &Ctx, room: &Room, follower: i64, body: &str) -> Result<i64> {
    let integration = bound_integration(ctx, &room.name.host)?;
    let from = room_address(ctx, &room.name, &integration)?;
    let local = db::namespace(ctx.conn)?;
    let origin = db::new_message_id();
    ctx.conn.execute(
        "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
             recipient_address_id, body, created_at, authored_at, group_copy_namespace,
             group_copy_message_id, group_copy_sender_address_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?1, ?2, ?3)",
        params![local, origin, from, follower, body, ctx.now_s],
    )?;
    let id = ctx.conn.last_insert_rowid();
    dbi::insert_delivery(ctx.conn, id, follower)?;
    Ok(id)
}

/// Deliver `follower`'s pending posts to its inbox, oldest first, for every
/// room or only `only`: at most [`MAX_PER_ROOM`] per room, the newest, with
/// one notice counting the rest (D7). Each is a message from the room naming
/// its author as the host reported it, outside ordinary Hub import (the
/// author may be local, and the post's expiry may have passed). Returns how
/// many it delivered.
pub(super) fn flush(ctx: &Ctx, follower: i64, only: Option<i64>) -> Result<usize> {
    let rooms: Vec<i64> = ctx
        .conn
        .prepare(
            "SELECT DISTINCT room_id FROM room_deliveries
             WHERE follower_address_id = ?1 AND state = 'pending' AND (?2 IS NULL OR room_id = ?2)",
        )?
        .query_map(params![follower, only], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut delivered = 0;
    for room_id in rooms {
        let room = room(ctx.conn, room_id)?;
        let pending: Vec<(i64, i64)> = ctx
            .conn
            .prepare(
                "SELECT d.id, d.post_id FROM room_deliveries d JOIN room_posts p ON p.id = d.post_id
                 WHERE d.follower_address_id = ?1 AND d.room_id = ?2 AND d.state = 'pending'
                 ORDER BY p.epoch, p.position, p.id",
            )?
            .query_map(params![follower, room_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let skip = pending.len().saturating_sub(MAX_PER_ROOM);
        if skip > 0 {
            for (id, _) in &pending[..skip] {
                ctx.conn.execute(
                    "UPDATE room_deliveries SET state = 'skipped' WHERE id = ?1",
                    [id],
                )?;
            }
            room_notice(
                ctx,
                &room,
                follower,
                &format!(
                    "{skip} earlier new posts in {} are not shown here; these are the newest \
                     {MAX_PER_ROOM}. `bbs room history {}` reads them.",
                    room.name.display(),
                    room.name.display()
                ),
            )?;
        }
        let integration = bound_integration(ctx, &room.name.host)?;
        let from = room_address(ctx, &room.name, &integration)?;
        for (id, post) in &pending[skip..] {
            let message = materialize(ctx, &room, &integration, from, *post, follower)?;
            ctx.conn.execute(
                "UPDATE room_deliveries SET state = 'delivered', message_id = ?2 WHERE id = ?1",
                params![id, message],
            )?;
            delivered += 1;
        }
    }
    Ok(delivered)
}

/// Store post `post` as a message from the room to `follower`, delivered.
fn materialize(
    ctx: &Ctx,
    _room: &Room,
    integration: &str,
    from: i64,
    post: i64,
    follower: i64,
) -> Result<i64> {
    let p = ctx.conn.query_row(
        "SELECT ref_namespace, ref_message_id, authored_at, author_namespace, author_entity_id,
                author_address_id, author_run_id, in_reply_to_namespace, in_reply_to_message_id,
                content_type, body, notice
         FROM room_posts WHERE id = ?1",
        [post],
        |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, String>(2)?,
                mp::AddressRef {
                    entity: Some(mp::EntityRef {
                        namespace: r.get(3)?,
                        entity_id: r.get(4)?,
                    }),
                    address_id: r.get(5)?,
                    run_id: Some(r.get::<_, String>(6)?).filter(|s| !s.is_empty()),
                },
                (
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ),
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, bool>(11)?,
            ))
        },
    )?;
    let (origin, authored, author, (reply_ns, reply_id), content_type, body, notice) = p;
    let by = if notice {
        from
    } else {
        author_address(ctx, &author, integration)?
    };
    let local = db::namespace(ctx.conn)?;
    ctx.conn.execute(
        "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
             recipient_address_id, body, content_type, created_at, authored_at,
             in_reply_to_namespace, in_reply_to_message_id, group_copy_namespace,
             group_copy_message_id, group_copy_sender_address_id, group_copy_sender_run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            local,
            db::new_message_id(),
            from,
            follower,
            body,
            content_type,
            ctx.now_s,
            authored,
            reply_ns,
            reply_id,
            origin.0,
            origin.1,
            by,
            author.run_id,
        ],
    )?;
    let id = ctx.conn.last_insert_rowid();
    dbi::insert_delivery(ctx.conn, id, follower)?;
    Ok(id)
}

/// Deliver what is due without waiting for the reader: tag deliveries at
/// once, and posts for `wake` follows at most once per room per minute
/// (D7). Returns how many it delivered.
pub(super) fn flush_due(ctx: &Ctx, cfg: &RoomConfig) -> Result<usize> {
    let mut delivered = 0;
    let tagged: Vec<(i64, i64)> = ctx
        .conn
        .prepare(
            "SELECT DISTINCT follower_address_id, room_id FROM room_deliveries
             WHERE state = 'pending' AND tag_wake = 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (follower, room) in tagged {
        if tag_may_wake(ctx, cfg, follower, room)? {
            delivered += flush(ctx, follower, Some(room))?;
        } else {
            // Over the tag budget: delivered quietly, at the next read.
            ctx.conn.execute(
                "UPDATE room_deliveries SET tag_wake = 0 WHERE follower_address_id = ?1
                 AND room_id = ?2 AND state = 'pending'",
                params![follower, room],
            )?;
        }
    }
    let since = fmt_time(ctx.now - cfg.d(cfg.timers.wake_coalesce));
    let woken: Vec<(i64, i64, i64)> = ctx
        .conn
        .prepare(
            "SELECT f.id, f.follower_address_id, f.room_id FROM room_follows f
             WHERE f.mode = 'wake' AND (f.last_wake_at IS NULL OR f.last_wake_at <= ?1)
               AND EXISTS (SELECT 1 FROM room_deliveries d WHERE d.follower_address_id =
                   f.follower_address_id AND d.room_id = f.room_id AND d.state = 'pending'
                   AND d.wake = 1)",
        )?
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (follow, follower, room) in woken {
        delivered += flush(ctx, follower, Some(room))?;
        ctx.conn.execute(
            "UPDATE room_follows SET last_wake_at = ?2 WHERE id = ?1",
            params![follow, ctx.now_s],
        )?;
    }
    Ok(delivered)
}

/// When the next coalesced wake falls due, if one waits.
pub(super) fn next_wake(ctx: &Ctx, cfg: &RoomConfig) -> Result<Option<String>> {
    let last: Option<String> = ctx.conn.query_row(
        "SELECT min(f.last_wake_at) FROM room_follows f
         WHERE f.mode = 'wake' AND f.last_wake_at IS NOT NULL
           AND EXISTS (SELECT 1 FROM room_deliveries d WHERE d.follower_address_id =
               f.follower_address_id AND d.room_id = f.room_id AND d.state = 'pending'
               AND d.wake = 1)",
        [],
        |r| r.get(0),
    )?;
    Ok(last.map(|t| {
        let t = chrono::DateTime::parse_from_rfc3339(&t)
            .map(|t| t.to_utc())
            .unwrap_or(ctx.now);
        fmt_time(t + cfg.d(cfg.timers.wake_coalesce))
    }))
}

/// Before a reader takes its mail, deliver its pending room posts (quiet
/// follows show at the next prompt). While a reader waits, they only come
/// with other mail, so a quiet follow never wakes it.
pub fn flush_for_reader(conn: &Connection, me: &db::Principal, waiting: bool) -> Result<usize> {
    let ctx = Ctx {
        conn,
        now: chrono::Utc::now(),
        now_s: fmt_time(chrono::Utc::now()),
        caller: &super::super::NO_CALLER,
    };
    let address = db::native_address_row(conn, me.kind.into(), me.id)?;
    let pending: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM room_deliveries
             WHERE follower_address_id = ?1 AND state = 'pending')
          OR EXISTS (SELECT 1 FROM room_mentions WHERE recipient_address_id = ?1 AND held = 1)",
        [address],
        |r| r.get(0),
    )?;
    if !pending {
        return Ok(0);
    }
    if waiting {
        let other: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM deliveries WHERE recipient_address_id = ?1
                 AND recipient_run_id = '' AND archived_at IS NULL)",
            [address],
            |r| r.get(0),
        )?;
        if !other {
            return Ok(0);
        }
    }
    let held: Vec<i64> = conn
        .prepare(
            "SELECT message_id FROM room_mentions WHERE recipient_address_id = ?1 AND held = 1",
        )?
        .query_map([address], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for message in &held {
        dbi::insert_delivery(conn, *message, address)?;
        conn.execute(
            "UPDATE room_mentions SET held = 0 WHERE message_id = ?1",
            [message],
        )?;
    }
    Ok(held.len() + flush(&ctx, address, None)?)
}

/// Mark inbox messages that came from rooms: their room, that the author is
/// the host's report, the tag merged into them, and tags that could not be
/// read back (unverified claims).
pub fn annotate(conn: &Connection, messages: &mut [proto::Message]) -> Result<()> {
    let mut room_of = conn.prepare(
        "SELECT se.entity_id || '@' || se.namespace FROM messages m
         JOIN entity_addresses sa ON sa.id = m.sender_address_id
         JOIN entities se ON se.id = sa.entity_id
         WHERE m.id = ?1 AND se.kind = 'room'",
    )?;
    let mut tagged = conn.prepare(&format!(
        "SELECT {} FROM room_deliveries d JOIN messages t ON t.id = d.tag_message_id
         JOIN entity_addresses ta ON ta.id = t.sender_address_id
         JOIN entities te ON te.id = ta.entity_id
         WHERE d.message_id = ?1",
        tagger_sql()
    ))?;
    let mut unverified = conn.prepare(
        "SELECT 'rooms/' || r.name || '@' || r.host FROM room_mentions x
         JOIN rooms r ON r.id = x.room_id WHERE x.message_id = ?1 AND x.state = 'unverified'",
    )?;
    for m in messages.iter_mut() {
        if let Some(room) = room_of
            .query_row([m.id], |r| r.get::<_, String>(0))
            .optional()?
        {
            m.room = Some(room);
            if m.original_from.is_some() {
                m.reported = Some(true);
            }
            m.room_tagged_by = tagged
                .query_row([m.id], |r| r.get::<_, String>(0))
                .optional()?;
        } else if let Some(room) = unverified
            .query_row([m.id], |r| r.get::<_, String>(0))
            .optional()?
        {
            m.room = Some(room);
            m.room_unverified = Some(true);
        }
    }
    Ok(())
}

/// SQL naming a tag's sender (`te`, `ta`): `PATH@NAMESPACE`.
fn tagger_sql() -> &'static str {
    "te.entity_id || '@' || te.namespace"
}

// ---- received tags (D10) -------------------------------------------------

/// A tag received: hold message `message` (already stored, not delivered)
/// for `recipient` until its post is read back from the room. Refused when
/// the mention does not name a room post.
pub fn hold_mention(
    conn: &Connection,
    message: i64,
    recipient: i64,
    mention: &mp::RoomMention,
    now: &str,
) -> Result<()> {
    let (Some(room), Some(post)) = (&mention.room, &mention.post) else {
        bail!("a mention names its room and post");
    };
    let Some(name) = room.entity_id.strip_prefix("rooms/") else {
        bail!("a mention's room is not under rooms/");
    };
    if !super::valid_name(name) || db::validate_namespace(&room.namespace).is_err() {
        bail!("a mention names an invalid room");
    }
    if post.namespace.is_empty() || post.message_id.is_empty() {
        bail!("a mention names an invalid post");
    }
    conn.execute(
        "INSERT INTO rooms (host, name, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(host, name) DO NOTHING",
        params![room.namespace, name, now],
    )?;
    let room_id: i64 = conn.query_row(
        "SELECT id FROM rooms WHERE host = ?1 AND name = ?2",
        params![room.namespace, name],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO room_mentions (message_id, recipient_address_id, room_id, post_namespace,
             post_message_id, state, next_check_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
        params![
            message,
            recipient,
            room_id,
            post.namespace,
            post.message_id,
            now
        ],
    )?;
    // A tag reads its room at once (D7).
    conn.execute(
        "UPDATE rooms SET next_fetch_at = ?2, kicks = kicks + 1
         WHERE id = ?1 AND next_fetch_at IS NOT NULL",
        params![room_id, now],
    )?;
    Ok(())
}

/// A held tag due a read-back.
pub(super) struct Mention {
    pub message: i64,
    pub recipient: i64,
    pub room: i64,
    pub post: mp::MessageRef,
    pub checks: i64,
}

pub(super) fn due_mentions(ctx: &Ctx) -> Result<Vec<Mention>> {
    Ok(ctx
        .conn
        .prepare(
            "SELECT message_id, recipient_address_id, room_id, post_namespace, post_message_id,
                    checks
             FROM room_mentions WHERE state = 'pending' AND next_check_at <= ?1
             ORDER BY room_id, message_id",
        )?
        .query_map([&ctx.now_s], |r| {
            Ok(Mention {
                message: r.get(0)?,
                recipient: r.get(1)?,
                room: r.get(2)?,
                post: mp::MessageRef {
                    namespace: r.get(3)?,
                    message_id: r.get(4)?,
                },
                checks: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// What a read-back found for one reference.
pub(super) enum Found<'a> {
    Present(&'a mp::RoomRecord),
    /// The host refused it, or holds no such post, or no such room: the
    /// host's answer, with its reason.
    Missing(String),
    /// No answer from the host.
    Unanswered,
}

/// Settle a held tag: read back, it merges into the post's delivery to its
/// recipient (one item, raised to a wake); not found, it is delivered as an
/// unverified claim; unanswered, it waits, up to the last retry.
pub(super) fn settle_mention(ctx: &Ctx, cfg: &RoomConfig, m: &Mention, found: Found) -> Result<()> {
    match found {
        Found::Present(rec) => {
            let room = room(ctx.conn, m.room)?;
            let f = decode_record(rec).context("an unreadable room record")?;
            let (post, _) = upsert_post(ctx, &room, &f)?;
            let state: Option<String> = ctx
                .conn
                .query_row(
                    "SELECT state FROM room_deliveries WHERE follower_address_id = ?1
                     AND post_id = ?2",
                    params![m.recipient, post],
                    |r| r.get(0),
                )
                .optional()?;
            match state.as_deref() {
                // Already shown: the tag itself is the news, verified.
                Some("delivered") | Some("skipped") => deliver_tag(ctx, cfg, m)?,
                // Merged into the post's delivery, which the tag may wake
                // (flush_due spends the budget).
                Some(_) => {
                    ctx.conn.execute(
                        "UPDATE room_deliveries SET tag_message_id = ?3, tag_wake = 1
                         WHERE follower_address_id = ?1 AND post_id = ?2",
                        params![m.recipient, post, m.message],
                    )?;
                }
                None => {
                    queue_delivery(ctx, room.id, m.recipient, post, "tag", false)?;
                    ctx.conn.execute(
                        "UPDATE room_deliveries SET tag_message_id = ?3, tag_wake = 1
                         WHERE follower_address_id = ?1 AND post_id = ?2",
                        params![m.recipient, post, m.message],
                    )?;
                }
            }
            ctx.conn.execute(
                "UPDATE room_mentions SET state = 'merged' WHERE message_id = ?1",
                [m.message],
            )?;
        }
        Found::Missing(why) => {
            tracing::info!(
                message = m.message,
                why,
                "a tag's post could not be read back"
            );
            unverified(ctx, cfg, m)?;
        }
        Found::Unanswered => {
            let checks = m.checks + 1;
            match cfg.timers.mention_retry.get(checks as usize) {
                Some(secs) => {
                    ctx.conn.execute(
                        "UPDATE room_mentions SET checks = ?2, next_check_at = ?3
                         WHERE message_id = ?1",
                        params![m.message, checks, fmt_time(ctx.now + cfg.d(*secs))],
                    )?;
                }
                None => unverified(ctx, cfg, m)?,
            }
        }
    }
    Ok(())
}

fn unverified(ctx: &Ctx, cfg: &RoomConfig, m: &Mention) -> Result<()> {
    ctx.conn.execute(
        "UPDATE room_mentions SET state = 'unverified' WHERE message_id = ?1",
        [m.message],
    )?;
    deliver_tag(ctx, cfg, m)
}

/// Deliver a settled tag's own message: at once if the tag budget lets it
/// wake its recipient, else held for the recipient's next read.
fn deliver_tag(ctx: &Ctx, cfg: &RoomConfig, m: &Mention) -> Result<()> {
    if tag_may_wake(ctx, cfg, m.recipient, m.room)? {
        dbi::insert_delivery(ctx.conn, m.message, m.recipient)?;
    } else {
        ctx.conn.execute(
            "UPDATE room_mentions SET held = 1 WHERE message_id = ?1",
            [m.message],
        )?;
    }
    Ok(())
}

/// The tag budget (D10): whether a tag may wake `recipient` for `room` now,
/// at most once per room per `wake_coalesce`; spends it when it may.
fn tag_may_wake(ctx: &Ctx, cfg: &RoomConfig, recipient: i64, room: i64) -> Result<bool> {
    let since = fmt_time(ctx.now - cfg.d(cfg.timers.wake_coalesce));
    let n = ctx.conn.execute(
        "INSERT INTO room_tag_wakes (recipient_address_id, room_id, woke_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(recipient_address_id, room_id) DO UPDATE SET woke_at = excluded.woke_at
         WHERE room_tag_wakes.woke_at <= ?4",
        params![recipient, room, ctx.now_s, since],
    )?;
    Ok(n == 1)
}

// ---- post outcomes (D9) and tags (D10) -----------------------------------

/// Record a room post just queued for its first hop, with its tags held.
pub(super) fn record_post(ctx: &Ctx, message: i64, room: i64, tags: &[String]) -> Result<()> {
    ctx.conn.execute(
        "INSERT INTO room_post_outcomes (message_id, room_id, created_at) VALUES (?1, ?2, ?3)",
        params![message, room, ctx.now_s],
    )?;
    for t in tags {
        ctx.conn.execute(
            "INSERT INTO room_tags (post_message_id, target, state) VALUES (?1, ?2, 'held')
             ON CONFLICT(post_message_id, target) DO NOTHING",
            params![message, t],
        )?;
    }
    Ok(())
}

/// A room post's outcome (D9) and its tags.
#[derive(Debug, Clone)]
pub(super) struct Outcome {
    pub message: i64,
    pub origin: mp::MessageRef,
    pub room: RoomName,
    pub state: &'static str,
    pub reason: Option<String>,
    pub position: Option<i64>,
    pub tags: Vec<(String, String)>,
}

impl Outcome {
    pub fn settled(&self) -> bool {
        matches!(self.state, "confirmed" | "rejected" | "expired")
    }
}

/// The outcome of the caller's room post with native ID `message`.
pub(super) fn outcome(conn: &Connection, message: i64) -> Result<Option<Outcome>> {
    let row = conn
        .query_row(
            "SELECT m.origin_namespace, m.origin_message_id, r.host, r.name, o.readback, o.reason,
                    o.position, x.state, x.detail
             FROM room_post_outcomes o JOIN messages m ON m.id = o.message_id
             JOIN rooms r ON r.id = o.room_id
             LEFT JOIN integration_outbox x ON x.message_id = o.message_id
             WHERE o.message_id = ?1",
            [message],
            |r| {
                Ok((
                    mp::MessageRef {
                        namespace: r.get(0)?,
                        message_id: r.get(1)?,
                    },
                    RoomName {
                        host: r.get(2)?,
                        name: r.get(3)?,
                    },
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((origin, room, readback, reason, position, handoff, detail)) = row else {
        return Ok(None);
    };
    let handoff_reason = || {
        detail
            .as_deref()
            .map(|d| d.split(':').next().unwrap_or(d).trim().to_string())
    };
    let (state, reason) = match (readback.as_deref(), handoff.as_deref()) {
        (Some("confirmed"), _) => ("confirmed", None),
        (Some("rejected"), _) => ("rejected", reason),
        (_, Some("rejected")) => match handoff_reason() {
            Some(r) if r == "expired" => ("expired", None),
            r => ("rejected", r),
        },
        (Some("unconfirmed"), _) => ("unconfirmed", None),
        (_, Some("pending" | "attempted") | None) => ("queued", None),
        (_, Some("unknown")) => ("unknown", None),
        (_, Some(_)) => ("sent", None),
    };
    let tags = conn
        .prepare("SELECT target, state FROM room_tags WHERE post_message_id = ?1 ORDER BY id")?
        .query_map([message], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(Some(Outcome {
        message,
        origin,
        room,
        state,
        reason,
        position,
        tags,
    }))
}

/// Room posts due a read-back: those past their first hop (sent, received
/// or unknown), not yet decided, or unconfirmed for less than a day.
pub(super) fn due_readbacks(
    ctx: &Ctx,
    cfg: &RoomConfig,
) -> Result<Vec<(i64, i64, mp::MessageRef)>> {
    let day_ago = fmt_time(ctx.now - cfg.d(cfg.timers.unconfirmed_for));
    Ok(ctx
        .conn
        .prepare(
            "SELECT o.message_id, o.room_id, m.origin_namespace, m.origin_message_id
             FROM room_post_outcomes o JOIN messages m ON m.id = o.message_id
             JOIN integration_outbox x ON x.message_id = o.message_id
             WHERE x.state IN ('sent', 'received', 'unknown')
               AND (o.readback IS NULL OR (o.readback = 'unconfirmed' AND o.unconfirmed_at > ?2))
               AND (o.next_check_at IS NULL OR o.next_check_at <= ?1)
             ORDER BY o.room_id, o.message_id",
        )?
        .query_map(params![ctx.now_s, day_ago], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                mp::MessageRef {
                    namespace: r.get(2)?,
                    message_id: r.get(3)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?)
}

/// Apply one read-back of post `message` (D9).
pub(super) fn settle_readback(
    ctx: &Ctx,
    cfg: &RoomConfig,
    message: i64,
    found: Found,
) -> Result<()> {
    let (checks, readback, last_absent, absent): (i64, Option<String>, Option<String>, i64) =
        ctx.conn.query_row(
            "SELECT checks, readback, last_absent_at, absent_micros FROM room_post_outcomes
             WHERE message_id = ?1",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let checks = checks + 1;
    let unconfirmed = readback.as_deref() == Some("unconfirmed");
    let next = if unconfirmed {
        cfg.d(cfg.timers.unconfirmed_every)
    } else {
        cfg.d(cfg.timers.readback_after(checks as usize))
    };
    let next_s = fmt_time(ctx.now + next);
    match found {
        Found::Present(rec) => {
            let position = i64::try_from(rec.position).ok();
            ctx.conn.execute(
                "UPDATE room_post_outcomes SET readback = 'confirmed', position = ?2, checks = ?3,
                     next_check_at = NULL WHERE message_id = ?1",
                params![message, position, checks],
            )?;
            if !unconfirmed {
                send_tags(ctx, message)?;
            }
        }
        Found::Missing(reason) if reason != "absent" => {
            ctx.conn.execute(
                "UPDATE room_post_outcomes SET readback = 'rejected', reason = ?2, checks = ?3,
                     next_check_at = NULL WHERE message_id = ?1",
                params![message, reason, checks],
            )?;
        }
        Found::Missing(_) => {
            let elapsed = last_absent
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                .map(|t| {
                    (ctx.now - t.to_utc())
                        .num_microseconds()
                        .unwrap_or(0)
                        .max(0)
                })
                .unwrap_or(0);
            let absent = absent + elapsed;
            ctx.conn.execute(
                "UPDATE room_post_outcomes SET checks = ?2, next_check_at = ?3,
                     last_absent_at = ?4, absent_micros = ?5 WHERE message_id = ?1",
                params![message, checks, next_s, ctx.now_s, absent],
            )?;
            let limit = cfg
                .d(cfg.timers.unconfirmed_after)
                .num_microseconds()
                .unwrap_or(i64::MAX);
            if !unconfirmed && absent >= limit {
                ctx.conn.execute(
                    "UPDATE room_post_outcomes SET readback = 'unconfirmed', unconfirmed_at = ?2,
                         next_check_at = ?3 WHERE message_id = ?1",
                    params![
                        message,
                        ctx.now_s,
                        fmt_time(ctx.now + cfg.d(cfg.timers.unconfirmed_every))
                    ],
                )?;
            }
        }
        // Time while the host is unreachable does not count (D9).
        Found::Unanswered => {
            ctx.conn.execute(
                "UPDATE room_post_outcomes SET checks = ?2, next_check_at = ?3,
                     last_absent_at = NULL WHERE message_id = ?1",
                params![message, checks, next_s],
            )?;
        }
    }
    settle_tags(ctx, message)
}

/// Drop the held tags of a post that ended rejected, expired or
/// unconfirmed, and tell its poster once.
pub(super) fn settle_tags(ctx: &Ctx, message: i64) -> Result<()> {
    let Some(o) = outcome(ctx.conn, message)? else {
        return Ok(());
    };
    if !matches!(o.state, "rejected" | "expired" | "unconfirmed") {
        return Ok(());
    }
    let held: Vec<String> = o
        .tags
        .iter()
        .filter(|(_, s)| s == "held")
        .map(|(t, _)| t.clone())
        .collect();
    let told: bool = ctx.conn.query_row(
        "SELECT settled_at IS NOT NULL FROM room_post_outcomes WHERE message_id = ?1",
        [message],
        |r| r.get(0),
    )?;
    if told {
        return Ok(());
    }
    ctx.conn.execute(
        "UPDATE room_tags SET state = 'dropped', detail = ?2 WHERE post_message_id = ?1
         AND state = 'held'",
        params![message, o.state],
    )?;
    ctx.conn.execute(
        "UPDATE room_post_outcomes SET settled_at = ?2 WHERE message_id = ?1",
        params![message, ctx.now_s],
    )?;
    let why = match (o.state, &o.reason) {
        ("rejected", Some(r)) => format!("was rejected ({r})"),
        ("rejected", None) => "was rejected".to_string(),
        ("expired", _) => "expired before it left this installation".to_string(),
        _ => "is unconfirmed: the host has not shown it for an hour of checks. It may still \
              arrive; repost it as a new message if it matters"
            .to_string(),
    };
    let tags = if held.is_empty() {
        String::new()
    } else {
        format!(" Its tags were not sent: {}.", held.join(", "))
    };
    poster_notice(
        ctx,
        message,
        &format!(
            "Your post {} to {} {why}.{tags}",
            super::ref_display(&o.origin),
            o.room.display()
        ),
    )
}

/// A notice to a post's sender, in its own inbox, answering the post.
fn poster_notice(ctx: &Ctx, message: i64, body: &str) -> Result<()> {
    let failed = ctx.conn.query_row(
        "SELECT m.id, m.sender_address_id, m.origin_namespace, m.origin_message_id, m.created_at
         FROM messages m WHERE m.id = ?1",
        [message],
        |r| {
            Ok(dbi::Failed {
                message: r.get(0)?,
                sender_address: r.get(1)?,
                origin_namespace: r.get(2)?,
                origin_message_id: r.get(3)?,
                created_at: r.get(4)?,
                recipient: String::new(),
            })
        },
    )?;
    dbi::insert_notice(ctx.conn, &failed, &db::new_message_id(), body, &ctx.now_s)?;
    Ok(())
}

/// The native principal whose address is `address`, if any.
fn principal_of(conn: &Connection, address: i64) -> Result<Option<db::Principal>> {
    Ok(conn
        .query_row(
            "SELECT b.agent_id, ag.name, b.human_id, h.name FROM entity_addresses a
             JOIN entity_local_bindings b ON b.entity_id = a.entity_id
             LEFT JOIN agents ag ON ag.id = b.agent_id LEFT JOIN humans h ON h.id = b.human_id
             WHERE a.id = ?1",
            [address],
            |r| {
                Ok(
                    match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(2)?) {
                        (Some(id), _) => Some(db::Principal {
                            kind: db::Kind::Agent,
                            id,
                            name: r.get(1)?,
                        }),
                        (None, Some(id)) => Some(db::Principal {
                            kind: db::Kind::Human,
                            id,
                            name: r.get(3)?,
                        }),
                        _ => None,
                    },
                )
            },
        )
        .optional()?
        .flatten())
}

/// Send a confirmed post's held tags (D10): a message to each target with a
/// `mention` of the post and a short body. A tag for one of this
/// installation's own entities is held for read-back like a received one.
pub(super) fn send_tags(ctx: &Ctx, message: i64) -> Result<()> {
    let held: Vec<(i64, String)> = ctx
        .conn
        .prepare("SELECT id, target FROM room_tags WHERE post_message_id = ?1 AND state = 'held'")?
        .query_map([message], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if held.is_empty() {
        return Ok(());
    }
    let o = outcome(ctx.conn, message)?.context("no outcome for a tagged post")?;
    let (sender, body): (i64, String) = ctx.conn.query_row(
        "SELECT sender_address_id, body FROM messages WHERE id = ?1",
        [message],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let Some(me) = principal_of(ctx.conn, sender)? else {
        bail!("a tagged post's sender is not native");
    };
    let mention = mp::RoomMention {
        room: Some(mp::EntityRef {
            namespace: o.room.host.clone(),
            entity_id: o.room.path(),
        }),
        post: Some(o.origin.clone()),
    };
    let excerpt: String = body
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    let text = format!(
        "{} tagged you in {} (post {}): {excerpt}",
        me.name,
        o.room.display(),
        super::ref_display(&o.origin)
    );
    let local = db::namespace(ctx.conn)?;
    for (id, target) in held {
        let reference = target.strip_prefix("id:").unwrap_or(&target);
        let sent = (|| -> Result<i64> {
            let remote = crate::entity::parse_remote(reference)?;
            if remote.address.entity.namespace == local {
                let found = db::local_entity(
                    ctx.conn,
                    &remote.address.entity.entity_id,
                    &remote.address.address_id,
                )?
                .and_then(|f| f.principal.filter(|_| f.active))
                .with_context(|| format!("no live local agent or human {target}"))?;
                // A tag is a direct message: focus mode applies as to any.
                super::super::check_focus(ctx, &me, Some(&found))?;
                let to = db::native_address_row(ctx.conn, found.kind.into(), found.id)?;
                let from = db::native_address_row(ctx.conn, me.kind.into(), me.id)?;
                ctx.conn.execute(
                    "INSERT INTO messages (origin_namespace, origin_message_id, sender_address_id,
                         recipient_address_id, body, created_at, authored_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![local, db::new_message_id(), from, to, text, ctx.now_s],
                )?;
                let tag = ctx.conn.last_insert_rowid();
                hold_mention(ctx.conn, tag, to, &mention, &ctx.now_s)?;
                Ok(tag)
            } else {
                let (tag, _) = integration::send_remote_with(
                    ctx,
                    &me,
                    reference,
                    &text,
                    None,
                    Some(mention.clone()),
                )?;
                Ok(tag)
            }
        })();
        match sent {
            Ok(tag) => ctx.conn.execute(
                "UPDATE room_tags SET state = 'sent', tag_message_id = ?2 WHERE id = ?1",
                params![id, tag],
            )?,
            Err(err) => ctx.conn.execute(
                "UPDATE room_tags SET state = 'dropped', detail = ?2 WHERE id = ?1",
                params![id, format!("{err:#}")],
            )?,
        };
    }
    Ok(())
}

/// Settle posts whose handoff already failed (rejected or expired before
/// leaving): their tags drop and their posters are told.
pub(super) fn settle_failed_handoffs(ctx: &Ctx) -> Result<()> {
    let failed: Vec<i64> = ctx
        .conn
        .prepare(
            "SELECT o.message_id FROM room_post_outcomes o
             JOIN integration_outbox x ON x.message_id = o.message_id
             WHERE x.state = 'rejected' AND o.settled_at IS NULL AND o.readback IS NULL",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for m in failed {
        settle_tags(ctx, m)?;
    }
    Ok(())
}
