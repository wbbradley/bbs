//! Rooms (DESIGN.md "Rooms"): shared, host-ordered chats on another home,
//! reached only through this installation's own integration with signed room
//! requests (msgproto `RoomRequest`/`RoomAnswer`, through the adapter).
//!
//! BBS hosts no rooms. It keeps what its own agents need, all in its
//! database: follows with one shared fetch cursor per room, the posts it
//! fetched, per-follower deliveries, durable operation IDs, post outcomes
//! and held tags. The async drivers here ([`command`] and [`poll_once`])
//! run against a [`RoomHome`]: the server, which owns the integration
//! stream, or a test's fake host.

use std::future::Future;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::hub_proto::integration as pb;
use crate::hub_proto::msgproto_v1 as mp;

mod command;
mod poll;
mod store;
#[cfg(test)]
mod tests;

pub use command::{VERBS, command};
pub use poll::poll_once;
pub use store::{annotate, flush_for_reader, hold_mention};

/// The entity this installation names as the requester of room reads made
/// for every follower at once (D7: a stable agent, not one that may end).
pub const FETCHER: &str = "bbs-room-fetcher";
/// The default host when a command names none and `BBS_ROOM_HOST` is unset.
pub const DEFAULT_HOST: &str = "hub";
/// The most tags one post may carry (D10).
pub const MAX_TAGS: usize = 8;
/// The most undelivered posts per room an agent is given at once (D7).
pub const MAX_PER_ROOM: usize = 50;
/// The most posts one backfill delivers (D7).
pub const MAX_BACKFILL: i64 = 500;
/// The most posts restore recovery reads back (D7).
pub const MAX_RECOVERY: usize = 1000;
/// The most references one read-back asks for (D2).
pub const MAX_REFS: usize = 32;

/// Where room work runs: a database, the integration that reaches hosts, and
/// a way to wake inbox watchers.
pub trait RoomHome: Send + Sync + 'static {
    /// Run `f` on the database.
    fn db<T, F>(&self, f: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static;
    /// Ask a room request through `integration`'s adapter. Every failure to
    /// get an answer is a result with `unavailable_code` set.
    fn ask(
        &self,
        integration: String,
        request: mp::RoomRequest,
    ) -> impl Future<Output = pb::RoomResult> + Send;
    /// Something committed that inbox watchers may want.
    fn changed(&self);
    fn now(&self) -> DateTime<Utc>;
}

/// The timers of D7 and D9, all scaled by `scale` (tests and scenario
/// harnesses shorten them), and the default host.
#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub scale: f64,
    pub default_host: String,
    /// Jitter on poll intervals, as a fraction either way (D7: 20%).
    pub jitter: f64,
}

impl Default for RoomConfig {
    fn default() -> RoomConfig {
        RoomConfig {
            scale: 1.0,
            default_host: DEFAULT_HOST.to_string(),
            jitter: 0.2,
        }
    }
}

impl RoomConfig {
    /// From the environment: `BBS_ROOM_TIMER_SCALE` (a positive factor on
    /// every room timer, e.g. 0.01) and `BBS_ROOM_HOST` (the default host).
    pub fn from_env() -> Result<RoomConfig> {
        let mut cfg = RoomConfig::default();
        if let Ok(scale) = std::env::var("BBS_ROOM_TIMER_SCALE") {
            let scale: f64 = scale
                .parse()
                .map_err(|_| anyhow::anyhow!("BBS_ROOM_TIMER_SCALE must be a number"))?;
            if !(scale > 0.0 && scale.is_finite()) {
                bail!("BBS_ROOM_TIMER_SCALE must be positive");
            }
            cfg.scale = scale;
        }
        if let Ok(host) = std::env::var("BBS_ROOM_HOST")
            && !host.is_empty()
        {
            crate::db::validate_namespace(&host)?;
            cfg.default_host = host;
        }
        Ok(cfg)
    }

    /// `secs` seconds, scaled.
    pub fn d(&self, secs: f64) -> chrono::Duration {
        chrono::Duration::microseconds((secs * self.scale * 1e6).max(1.0) as i64)
    }

    /// `secs` seconds, scaled, with jitter.
    fn jittered(&self, secs: f64) -> chrono::Duration {
        let j = if self.jitter > 0.0 {
            rand::random_range(1.0 - self.jitter..=1.0 + self.jitter)
        } else {
            1.0
        };
        self.d(secs * j)
    }
}

// The schedules (unscaled seconds).
const POLL_WAKE: f64 = 15.0;
const POLL_QUIET: f64 = 60.0;
const POLL_IDLE: f64 = 300.0;
const IDLE_AFTER: f64 = 3600.0;
const POLL_ARCHIVED: f64 = 3600.0;
const BACKOFF_MAX: f64 = 900.0;
const WAKE_COALESCE: f64 = 60.0;
/// Read-back after the handoff result, then at these delays, then every
/// last one (D9).
const READBACK: [f64; 5] = [0.0, 10.0, 60.0, 300.0, 900.0];
const ABSENT_LIMIT: f64 = 3600.0;
const UNCONFIRMED_EVERY: f64 = 3600.0;
const UNCONFIRMED_FOR: f64 = 86400.0;
/// Tag read-backs; after the last unanswered one the tag is delivered as an
/// unverified claim.
const MENTION_RETRY: [f64; 4] = [0.0, 10.0, 60.0, 300.0];

/// Hub's room name grammar: one segment, `[a-z0-9][a-z0-9-]{0,62}`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A room as a command names it: `NAME`, `rooms/NAME` or `rooms/NAME@HOST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomName {
    pub host: String,
    pub name: String,
}

impl RoomName {
    pub fn parse(text: &str, host: Option<&str>, default_host: &str) -> Result<RoomName> {
        let text = text.trim();
        let (path, at) = match text.rsplit_once('@') {
            Some((path, at)) => (path, Some(at)),
            None => (text, None),
        };
        let name = path.strip_prefix("rooms/").unwrap_or(path);
        if !valid_name(name) {
            bail!(
                "invalid room {text:?}: expected NAME, rooms/NAME or rooms/NAME@HOST, where \
                 NAME is [a-z0-9][a-z0-9-]{{0,62}}"
            );
        }
        let host = match (at, host) {
            (Some(a), Some(h)) if a != h => {
                bail!("room {text:?} names host {a}, but --host is {h}")
            }
            (Some(a), _) => a,
            (None, Some(h)) => h,
            (None, None) => default_host,
        };
        crate::db::validate_namespace(host)?;
        Ok(RoomName {
            host: host.to_string(),
            name: name.to_string(),
        })
    }

    /// The room's path on its host, `rooms/NAME`: its entity ID there.
    pub fn path(&self) -> String {
        format!("rooms/{}", self.name)
    }

    /// `rooms/NAME@HOST`, the address posts go to.
    pub fn display(&self) -> String {
        format!("rooms/{}@{}", self.name, self.host)
    }
}

/// A message reference as `NAMESPACE:MESSAGE_ID`.
pub fn ref_display(r: &mp::MessageRef) -> String {
    format!("{}:{}", r.namespace, r.message_id)
}

/// An agent's address as Hub writes it: `PATH@NAMESPACE`, with `:ADDRESS`
/// when not the native one and `#RUN` with a run.
pub fn address_display(a: &mp::AddressRef) -> String {
    let e = a.entity.clone().unwrap_or_default();
    let mut s = format!("{}@{}", e.entity_id, e.namespace);
    if !a.address_id.is_empty() && a.address_id != crate::entity::NATIVE_ADDRESS {
        s.push(':');
        s.push_str(&a.address_id);
    }
    if let Some(run) = &a.run_id {
        s.push('#');
        s.push_str(run);
    }
    s
}

pub(crate) fn b64(b: &[u8]) -> String {
    data_encoding::BASE64URL_NOPAD.encode(b)
}

pub(crate) fn unb64(s: &str) -> Result<Vec<u8>> {
    data_encoding::BASE64URL_NOPAD
        .decode(s.trim().as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid cursor {s:?}"))
}
