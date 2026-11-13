//! Rooms on the server (DESIGN.md "Rooms"): the `Room` RPC, which runs a
//! `bbs room` verb, and the poller, which keeps followed rooms read and
//! posts read back. Both run `service::rooms` against this server: its
//! database, and its integration streams for room requests.

use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use tonic::Status;

use super::{State, integration};
use crate::service::rooms::{self, RoomConfig, RoomHome};
use bbs_core::hub_proto::integration as pb;
use bbs_core::hub_proto::msgproto_v1 as mp;

/// The room timers and default host, from the environment once.
pub(super) fn config() -> &'static RoomConfig {
    static CONFIG: OnceLock<RoomConfig> = OnceLock::new();
    CONFIG.get_or_init(|| match RoomConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!("room settings: {err:#}; using the defaults");
            RoomConfig::default()
        }
    })
}

/// This server as rooms see it.
pub(super) struct Home(pub Arc<State>);

impl RoomHome for Home {
    async fn db<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let state = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
            if *state.shutdown.borrow() {
                anyhow::bail!("server is shutting down");
            }
            f(&mut conn)
        })
        .await?
    }

    async fn ask(&self, integration: String, request: mp::RoomRequest) -> pb::RoomResult {
        integration::remote_room(&self.0, &integration, request).await
    }

    fn changed(&self) {
        self.0.changed();
    }

    fn now(&self) -> DateTime<Utc> {
        (self.0.clock)()
    }
}

/// The status for a failed room command.
pub(super) fn status(err: anyhow::Error) -> Status {
    if err
        .downcast_ref::<crate::auth::AuthenticationError>()
        .is_some()
    {
        Status::unauthenticated(err.to_string())
    } else if let Some(e) = err.downcast_ref::<crate::service::integration::InvalidRequest>() {
        Status::invalid_argument(e.to_string())
    } else {
        Status::failed_precondition(format!("{err:#}"))
    }
}

/// The longest the poller sleeps when nothing is due, and the shortest gap
/// between passes woken by changes.
const IDLE: Duration = Duration::from_secs(30);
const MIN_GAP: Duration = Duration::from_millis(50);

/// Poll followed rooms, read back posts and tags, and deliver what is due,
/// until the server stops. A committed change (a post, a follow, a handoff
/// result) wakes it early.
pub(super) async fn run(state: Arc<State>) {
    let home = Home(state.clone());
    let cfg = config().clone();
    let mut changes = state.notify.subscribe();
    loop {
        changes.borrow_and_update();
        let next = match rooms::poll_once(&home, &cfg).await {
            Ok(next) => next,
            Err(err) => {
                if *state.shutdown.borrow() {
                    return;
                }
                tracing::warn!("room poller: {err:#}");
                None
            }
        };
        let wait = next
            .map(|t| (t - (state.clock)()).to_std().unwrap_or(Duration::ZERO))
            .unwrap_or(IDLE)
            .min(IDLE)
            .max(MIN_GAP);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = changes.changed() => tokio::time::sleep(MIN_GAP).await,
            _ = state.stopping() => return,
        }
    }
}
