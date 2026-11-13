//! The room poller (D7, D9, D10): one pass reads every followed room that is
//! due through the shared fetch cursor, runs due backfills, reads back
//! pending posts and received tags, and delivers what is due. Every read is
//! made as [`FETCHER`], one stable entity for the whole installation.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::super::{Ctx, NO_CALLER, fmt_time, with_ctx_at};
use super::store::{self, Found};
use super::{FETCHER, MAX_RECOVERY, MAX_REFS, RoomConfig, RoomHome, RoomName};
use crate::db;
use crate::hub_proto::integration as pb;
use crate::hub_proto::msgproto_v1 as mp;

/// How many AFTER pages one pass reads from a busy room.
const PAGES_PER_PASS: usize = 5;
/// Records per page the poller asks for.
const PAGE: u32 = 200;
/// How long a room request may take (D2: 10 s by default).
const DEADLINE: chrono::Duration = chrono::Duration::seconds(10);

/// What a room request came back with.
pub(super) enum Answer {
    /// The host answered without an error.
    Ok(mp::RoomAnswer),
    /// The host answered with an error code.
    Refused(String, String, mp::RoomAnswer),
    /// No answer: "no information" (RoomUnavailable, a timeout, a local
    /// verification failure).
    Unavailable(String, String),
}

pub(super) fn classify(r: pb::RoomResult) -> Answer {
    match r.answer {
        Some(a) if r.unavailable_code.is_empty() => {
            if a.error_code.is_empty() {
                Answer::Ok(a)
            } else {
                Answer::Refused(a.error_code.clone(), a.detail.clone(), a)
            }
        }
        _ => Answer::Unavailable(
            if r.unavailable_code.is_empty() {
                "relay_unavailable".to_string()
            } else {
                r.unavailable_code
            },
            r.detail,
        ),
    }
}

/// A room request about `room` (empty for list and create).
pub(super) fn request(
    requester: mp::EntityRef,
    host: &str,
    room: &str,
    now: DateTime<Utc>,
    op: mp::room_request::Op,
) -> mp::RoomRequest {
    mp::RoomRequest {
        request_id: db::new_message_id(),
        requester: Some(mp::Requester {
            entity: Some(requester),
            run_id: None,
        }),
        namespace: host.to_string(),
        room: room.to_string(),
        deadline_unix_micros: (now + DEADLINE).timestamp_micros(),
        op: Some(op),
    }
}

fn read(mode: mp::RoomReadMode, cursor: Vec<u8>, limit: u32) -> mp::room_request::Op {
    mp::room_request::Op::Read(mp::RoomRead {
        mode: mode as i32,
        cursor,
        limit,
        ..Default::default()
    })
}

/// Run `f` in one transaction at the home's clock.
async fn tx<H, T, F>(home: &H, f: F) -> Result<T>
where
    H: RoomHome,
    T: Send + 'static,
    F: FnOnce(&Ctx) -> Result<T> + Send + 'static,
{
    let now = home.now();
    home.db(move |conn| with_ctx_at(conn, &NO_CALLER, now, f))
        .await
}

/// The fetcher's entity.
fn fetcher(conn: &Connection) -> Result<mp::EntityRef> {
    Ok(mp::EntityRef {
        namespace: db::namespace(conn)?,
        entity_id: FETCHER.to_string(),
    })
}

/// The record in `page` for reference `r`.
pub(super) fn record_for<'a>(
    page: &'a mp::RoomPage,
    r: &mp::MessageRef,
) -> Option<&'a mp::RoomRecord> {
    page.records.iter().find(|rec| {
        store::decode_record(rec)
            .and_then(|f| f.message.id)
            .is_some_and(|id| id == *r)
    })
}

/// What a REFS read found for each of `refs`.
pub(super) fn found<'a>(answer: &'a Answer, r: &mp::MessageRef) -> Found<'a> {
    match answer {
        Answer::Ok(a) => {
            let Some(mp::room_answer::Result::Page(page)) = &a.result else {
                return Found::Unanswered;
            };
            let Some(result) = page.refs.iter().find(|x| x.r#ref.as_ref() == Some(r)) else {
                return Found::Unanswered;
            };
            match mp::RoomRefStatus::try_from(result.status) {
                Ok(mp::RoomRefStatus::Present) => match record_for(page, r) {
                    Some(rec) => Found::Present(rec),
                    // Present but over the page's byte budget: ask again.
                    None => Found::Unanswered,
                },
                Ok(mp::RoomRefStatus::Refused) => Found::Missing(if result.reason.is_empty() {
                    "refused".to_string()
                } else {
                    result.reason.clone()
                }),
                Ok(mp::RoomRefStatus::Absent) => Found::Missing("absent".to_string()),
                _ => Found::Unanswered,
            }
        }
        // The room itself is gone: nothing in it can be read back.
        Answer::Refused(code, _, _) if matches!(code.as_str(), "not_found" | "not_a_room") => {
            Found::Missing(code.clone())
        }
        _ => Found::Unanswered,
    }
}

/// One pass of room work. Returns when the next is due, if anything waits.
pub async fn poll_once<H: RoomHome>(home: &H, cfg: &RoomConfig) -> Result<Option<DateTime<Utc>>> {
    let due: Vec<i64> = tx(home, |ctx| {
        Ok(ctx
            .conn
            .prepare(
                "SELECT r.id FROM rooms r WHERE r.ended IS NULL
                   AND EXISTS (SELECT 1 FROM room_follows f WHERE f.room_id = r.id)
                   AND (r.next_fetch_at IS NULL OR r.next_fetch_at <= ?1)
                 ORDER BY r.id",
            )?
            .query_map([&ctx.now_s], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    })
    .await?;
    for room in due {
        if let Err(err) = fetch(home, cfg, room).await {
            tracing::warn!(room, "reading a followed room: {err:#}");
        }
    }
    let backfills = tx(home, |ctx| store::backfills(ctx.conn)).await?;
    for b in backfills {
        if let Err(err) = backfill(home, b).await {
            tracing::warn!("a room backfill: {err:#}");
        }
    }
    if let Err(err) = readbacks(home, cfg).await {
        tracing::warn!("reading back room posts: {err:#}");
    }
    if let Err(err) = mentions(home, cfg).await {
        tracing::warn!("reading back room tags: {err:#}");
    }
    let cfg2 = cfg.clone();
    let delivered = tx(home, move |ctx| {
        store::settle_failed_handoffs(ctx)?;
        store::flush_due(ctx, &cfg2)
    })
    .await?;
    if delivered > 0 {
        home.changed();
    }
    let cfg2 = cfg.clone();
    tx(home, move |ctx| next_due(ctx, &cfg2)).await
}

/// When the next pass has work.
fn next_due(ctx: &Ctx, cfg: &RoomConfig) -> Result<Option<DateTime<Utc>>> {
    let day_ago = fmt_time(ctx.now - cfg.d(super::UNCONFIRMED_FOR));
    let times: Vec<Option<String>> = vec![
        ctx.conn.query_row(
            "SELECT min(coalesce(r.next_fetch_at, ?1)) FROM rooms r WHERE r.ended IS NULL
               AND EXISTS (SELECT 1 FROM room_follows f WHERE f.room_id = r.id)",
            [&ctx.now_s],
            |r| r.get(0),
        )?,
        ctx.conn.query_row(
            "SELECT min(coalesce(o.next_check_at, ?1)) FROM room_post_outcomes o
             JOIN integration_outbox x ON x.message_id = o.message_id
             WHERE x.state IN ('sent', 'received', 'unknown')
               AND (o.readback IS NULL OR (o.readback = 'unconfirmed' AND o.unconfirmed_at > ?2))",
            params![ctx.now_s, day_ago],
            |r| r.get(0),
        )?,
        ctx.conn.query_row(
            "SELECT min(next_check_at) FROM room_mentions WHERE state = 'pending'",
            [],
            |r| r.get(0),
        )?,
        store::next_wake(ctx, cfg)?,
        // A backfill in progress continues at once.
        ctx.conn.query_row(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM room_follows f JOIN rooms r ON r.id = f.room_id
                 WHERE f.backfill_until IS NOT NULL AND f.epoch = r.epoch AND r.ended IS NULL)
             THEN ?1 END",
            [&ctx.now_s],
            |r| r.get(0),
        )?,
    ];
    Ok(times
        .into_iter()
        .flatten()
        .filter_map(|t| DateTime::parse_from_rfc3339(&t).ok().map(|t| t.to_utc()))
        .min())
}

/// The room with row `room`, its integration (or why there is none) and the
/// fetcher's entity.
async fn load<H: RoomHome>(
    home: &H,
    room: i64,
) -> Result<(store::Room, Result<String, String>, mp::EntityRef)> {
    tx(home, move |ctx| {
        let r = store::room(ctx.conn, room)?;
        let integration = store::integration_for(ctx, &r.name.host).map_err(|e| format!("{e:#}"));
        Ok((r, integration, fetcher(ctx.conn)?))
    })
    .await
}

/// Ask `op` about `room` as `requester`; no integration is no answer.
async fn ask<H: RoomHome>(
    home: &H,
    integration: &Result<String, String>,
    requester: mp::EntityRef,
    room: &RoomName,
    op: mp::room_request::Op,
) -> Answer {
    match integration {
        Ok(i) => {
            let req = request(requester, &room.host, &room.path(), home.now(), op);
            classify(home.ask(i.clone(), req).await)
        }
        Err(why) => Answer::Unavailable("relay_unavailable".to_string(), why.clone()),
    }
}

/// Read one followed room through its shared cursor (D7).
async fn fetch<H: RoomHome>(home: &H, cfg: &RoomConfig, id: i64) -> Result<()> {
    let seen = tx(home, move |ctx| store::kicks(ctx.conn, id)).await?;
    let done = fetch_pages(home, cfg, id).await;
    tx(home, move |ctx| store::keep_kick(ctx, id, seen)).await?;
    done
}

async fn fetch_pages<H: RoomHome>(home: &H, cfg: &RoomConfig, id: i64) -> Result<()> {
    let (room, integration, me) = load(home, id).await?;
    let Some(cursor) = room.fetch_cursor.clone() else {
        let answer = ask(
            home,
            &integration,
            me,
            &room.name,
            read(mp::RoomReadMode::Latest, Vec::new(), 1),
        )
        .await;
        return match answer {
            Answer::Ok(a) => {
                let Some(mp::room_answer::Result::Page(page)) = a.result else {
                    return unanswered(home, cfg, id, "the answer has no page").await;
                };
                let cfg = cfg.clone();
                tx(home, move |ctx| store::start_fetch(ctx, &cfg, &room, &page)).await
            }
            Answer::Refused(code, detail, _) => refused(home, cfg, room, &code, &detail).await,
            Answer::Unavailable(code, detail) => {
                unanswered(home, cfg, id, &format!("{code}: {detail}")).await
            }
        };
    };
    let mut cursor = cursor;
    for _ in 0..PAGES_PER_PASS {
        let answer = ask(
            home,
            &integration,
            me.clone(),
            &room.name,
            read(mp::RoomReadMode::After, cursor.clone(), PAGE),
        )
        .await;
        match answer {
            Answer::Ok(a) => {
                let Some(mp::room_answer::Result::Page(page)) = a.result else {
                    return unanswered(home, cfg, id, "the answer has no page").await;
                };
                let more = page.records.len() >= PAGE as usize && page.next_cursor != cursor;
                cursor = page.next_cursor.clone();
                let cfg = cfg.clone();
                let queued = tx(home, move |ctx| {
                    let room = store::room(ctx.conn, id)?;
                    store::apply_page(ctx, &cfg, &room, &page)
                })
                .await?;
                if queued > 0 {
                    home.changed();
                }
                if !more {
                    return Ok(());
                }
            }
            Answer::Refused(code, _, _) if code == "cursor_invalid" => {
                return recover(home, cfg, id).await;
            }
            Answer::Refused(code, detail, _) => {
                return refused(home, cfg, room, &code, &detail).await;
            }
            Answer::Unavailable(code, detail) => {
                return unanswered(home, cfg, id, &format!("{code}: {detail}")).await;
            }
        }
    }
    Ok(())
}

async fn unanswered<H: RoomHome>(home: &H, cfg: &RoomConfig, room: i64, why: &str) -> Result<()> {
    tracing::info!(room, "no answer reading a followed room: {why}");
    let cfg = cfg.clone();
    tx(home, move |ctx| store::fetch_unanswered(ctx, &cfg, room)).await
}

/// The host answered a fetch with an error: not_found, not_a_room and
/// forbidden end the room's follows; others back off.
async fn refused<H: RoomHome>(
    home: &H,
    cfg: &RoomConfig,
    room: store::Room,
    code: &str,
    detail: &str,
) -> Result<()> {
    if matches!(code, "not_found" | "not_a_room" | "forbidden") {
        let (code, detail) = (code.to_string(), detail.to_string());
        tx(home, move |ctx| {
            store::end_follows(ctx, &room, &code, &detail)
        })
        .await?;
        home.changed();
        return Ok(());
    }
    unanswered(home, cfg, room.id, &format!("{code}: {detail}")).await
}

/// Restore recovery (D7): note the head, read backward from it to a post
/// already fetched or [`MAX_RECOVERY`] posts, then apply it all at once.
async fn recover<H: RoomHome>(home: &H, cfg: &RoomConfig, id: i64) -> Result<()> {
    let (room, integration, me) = load(home, id).await?;
    let mut records: Vec<mp::RoomRecord> = Vec::new();
    let mut head = None;
    let mut cursor = Vec::new();
    let mut mode = mp::RoomReadMode::Latest;
    let mut bounded = false;
    loop {
        let answer = ask(
            home,
            &integration,
            me.clone(),
            &room.name,
            read(mode, cursor.clone(), 100),
        )
        .await;
        let page = match answer {
            Answer::Ok(a) => match a.result {
                Some(mp::room_answer::Result::Page(p)) => p,
                _ => return unanswered(home, cfg, id, "the answer has no page").await,
            },
            Answer::Refused(code, detail, _) => {
                return if code == "cursor_invalid" {
                    // The log changed again under us: start over next pass.
                    unanswered(home, cfg, id, &format!("{code}: {detail}")).await
                } else {
                    refused(home, cfg, room, &code, &detail).await
                };
            }
            Answer::Unavailable(code, detail) => {
                return unanswered(home, cfg, id, &format!("{code}: {detail}")).await;
            }
        };
        if head.is_none() {
            head = Some((
                i64::try_from(page.head_position).unwrap_or(i64::MAX),
                page.head_cursor.clone(),
            ));
        }
        let refs: Vec<mp::MessageRef> = page
            .records
            .iter()
            .filter_map(|r| store::decode_record(r).and_then(|f| f.message.id))
            .collect();
        let known = tx(home, move |ctx| {
            for r in &refs {
                if store::known_post(ctx.conn, id, r)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await?;
        let empty = page.records.is_empty();
        records.extend(page.records);
        if known || empty || page.next_cursor.is_empty() || page.next_cursor == cursor {
            break;
        }
        if records.len() >= MAX_RECOVERY {
            bounded = true;
            break;
        }
        cursor = page.next_cursor;
        mode = mp::RoomReadMode::Before;
    }
    let (head, head_cursor) = head.unwrap_or_default();
    let cfg = cfg.clone();
    let queued = tx(home, move |ctx| {
        let room = store::room(ctx.conn, id)?;
        store::recover(ctx, &cfg, &room, head, &head_cursor, &records, bounded)
    })
    .await?;
    if queued > 0 {
        home.changed();
    }
    home.changed();
    Ok(())
}

/// One page of a follow's backfill (D7).
async fn backfill<H: RoomHome>(home: &H, b: store::Backfill) -> Result<()> {
    let (room, integration, me) = load(home, b.room).await?;
    let answer = ask(
        home,
        &integration,
        me,
        &room.name,
        read(mp::RoomReadMode::After, b.cursor.clone(), PAGE),
    )
    .await;
    match answer {
        Answer::Ok(a) => {
            let Some(mp::room_answer::Result::Page(page)) = a.result else {
                return Ok(());
            };
            tx(home, move |ctx| store::apply_backfill(ctx, &b, &page)).await?;
        }
        Answer::Refused(code, detail, _) => {
            let why = format!("the host answered {code}: {detail}");
            tx(home, move |ctx| store::end_backfill(ctx, &b, &why)).await?;
            home.changed();
        }
        // Tried again on the next pass.
        Answer::Unavailable(..) => {}
    }
    Ok(())
}

/// Read back pending posts, batched per room (D9).
async fn readbacks<H: RoomHome>(home: &H, cfg: &RoomConfig) -> Result<()> {
    let cfg2 = cfg.clone();
    let due = tx(home, move |ctx| store::due_readbacks(ctx, &cfg2)).await?;
    let mut by_room: BTreeMap<i64, Vec<(i64, mp::MessageRef)>> = BTreeMap::new();
    for (message, room, r) in due {
        by_room.entry(room).or_default().push((message, r));
    }
    for (room, posts) in by_room {
        for chunk in posts.chunks(MAX_REFS) {
            let (r, integration, me) = load(home, room).await?;
            let refs: Vec<mp::MessageRef> = chunk.iter().map(|(_, r)| r.clone()).collect();
            let op = mp::room_request::Op::Read(mp::RoomRead {
                mode: mp::RoomReadMode::Refs as i32,
                refs,
                ..Default::default()
            });
            let answer = ask(home, &integration, me, &r.name, op).await;
            let chunk = chunk.to_vec();
            let cfg = cfg.clone();
            let answer = std::sync::Arc::new(answer);
            tx(home, move |ctx| {
                for (message, r) in &chunk {
                    store::settle_readback(ctx, &cfg, *message, found(&answer, r))?;
                }
                Ok(())
            })
            .await?;
        }
    }
    Ok(())
}

/// Read back the posts received tags point at (D10).
async fn mentions<H: RoomHome>(home: &H, cfg: &RoomConfig) -> Result<()> {
    let due = tx(home, store::due_mentions).await?;
    let mut by_room: BTreeMap<i64, Vec<store::Mention>> = BTreeMap::new();
    for m in due {
        by_room.entry(m.room).or_default().push(m);
    }
    let mut any = false;
    for (room, ms) in by_room {
        for chunk in ms.chunks(MAX_REFS) {
            let (r, integration, me) = load(home, room).await?;
            let refs: Vec<mp::MessageRef> = chunk.iter().map(|m| m.post.clone()).collect();
            let op = mp::room_request::Op::Read(mp::RoomRead {
                mode: mp::RoomReadMode::Refs as i32,
                refs,
                ..Default::default()
            });
            let answer = std::sync::Arc::new(ask(home, &integration, me, &r.name, op).await);
            let chunk: Vec<(i64, i64, i64, mp::MessageRef, i64)> = chunk
                .iter()
                .map(|m| (m.message, m.recipient, m.room, m.post.clone(), m.checks))
                .collect();
            let cfg = cfg.clone();
            tx(home, move |ctx| {
                for (message, recipient, room, post, checks) in chunk {
                    let m = store::Mention {
                        message,
                        recipient,
                        room,
                        post,
                        checks,
                    };
                    store::settle_mention(ctx, &cfg, &m, found(&answer, &m.post))?;
                }
                Ok(())
            })
            .await?;
            any = true;
        }
    }
    if any {
        home.changed();
    }
    Ok(())
}
