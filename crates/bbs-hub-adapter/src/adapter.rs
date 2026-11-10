//! The running adapter: its store, keys and the two streams it keeps open,
//! one to BBS's integration listener ([`crate::bbs`]) and one to the Hub
//! relay ([`crate::relay`]). Each side reconnects on its own with jittered
//! exponential backoff; work for a side that is down stays pending in the
//! store.
//!
//! Lock order, outermost first: the store; then a link's queue (see
//! [`crate::link`]); then the boundary's event list, a leaf. The `bbs`,
//! `relay` and `directory` slots are only held to read or swap them, taking
//! nothing else. Nothing takes the store inside a queue lock: what a
//! transport takes is recorded as an event and applied to the store by
//! [`Shared::apply_boundary`], outside every queue lock (`Shared::store`
//! checks this in debug builds).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bbs_core::hub_proto::integration as pb;
use bbs_core::hub_proto::msgproto_v1 as mp;
use ed25519_dalek::SigningKey;
use tokio::sync::{Notify, oneshot, watch};

use crate::config::Config;
use crate::link::{self, Claim, Refused, Settle};
use crate::store::{Direction, Outcome, Store};
use crate::{keys, now_micros, verify};

/// Timing knobs; the defaults are Hub's (CONTRACT.md).
#[derive(Debug, Clone)]
pub struct Options {
    /// How often a heartbeat goes to the relay.
    pub heartbeat: Duration,
    /// How long the relay may be silent before the stream is cut.
    pub peer_timeout: Duration,
    /// How long a hello, a trust page or a route result may take.
    pub reply_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// How often expired pending work is settled.
    pub sweep: Duration,
    /// How often idle dispatch looks for work it was not told about.
    pub poll: Duration,
    /// How long settled handoffs are kept; None keeps them.
    pub retention: Option<Duration>,
    /// How long shutdown lets the relay's transport take frames already
    /// queued (CONTRACT.md: bounded drain, up to 10 seconds).
    pub drain: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            heartbeat: Duration::from_secs(15),
            peer_timeout: Duration::from_secs(45),
            reply_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(30),
            sweep: Duration::from_secs(30),
            poll: Duration::from_secs(1),
            retention: None,
            drain: Duration::from_secs(10),
        }
    }
}

/// Everything the adapter needs, loaded and checked.
pub struct Settings {
    pub namespace: String,
    pub installation_id: String,
    pub integration_id: String,
    pub bbs_endpoint: String,
    pub relay_endpoint: String,
    /// BBS's integration credential. Never logged.
    pub token: String,
    /// Where it came from, to read again when BBS refuses it.
    pub token_file: Option<std::path::PathBuf>,
    /// The installation key. Never logged.
    pub key: SigningKey,
    /// This installation's credential, as the namespace authority signed it.
    pub credential: mp::SignedRecord,
}

impl Settings {
    /// Load the keys and credentials a configuration names, checking file
    /// permissions and that the credential matches.
    pub fn load(cfg: &Config) -> Result<Settings> {
        let key = keys::read_key(&cfg.hub.key_file)?;
        let (credential, _) = keys::read_credential(
            &cfg.hub.credential_file,
            &cfg.namespace,
            &cfg.hub.installation_id,
            &key,
        )?;
        Ok(Settings {
            namespace: cfg.namespace.clone(),
            installation_id: cfg.hub.installation_id.clone(),
            integration_id: cfg.bbs.integration_id.clone(),
            bbs_endpoint: cfg.bbs.endpoint.clone(),
            relay_endpoint: cfg.hub.relay.clone(),
            token: keys::read_token(&cfg.bbs.credential_file)?,
            token_file: Some(cfg.bbs.credential_file.clone()),
            key,
            credential,
        })
    }
}

/// The relay stream currently usable for transfers.
#[derive(Clone)]
pub(crate) struct RelayLink {
    pub tx: link::Sender<Vec<u8>>,
    /// Our hello's connection ID on that stream.
    pub connection: String,
}

/// The most directory queries waiting on BBS at once.
pub(crate) const MAX_DIRECTORY_QUERIES: usize = 128;

pub(crate) struct Shared {
    pub s: Settings,
    pub opts: Options,
    store: Mutex<Store>,
    pub bbs: Mutex<Option<link::Sender<pb::AdapterFrame>>>,
    pub relay: Mutex<Option<RelayLink>>,
    /// Wakes outbound dispatch to the relay.
    pub relay_kick: Notify,
    /// Wakes inbound dispatch to BBS.
    pub bbs_kick: Notify,
    /// Wakes the reporting of results owed to BBS.
    pub report_kick: Notify,
    /// Wakes the reporting of import results to the relay.
    pub relay_report_kick: Notify,
    /// What transports took or returned, waiting to be recorded.
    boundary: Mutex<Vec<(bool, Claim)>>,
    boundary_kick: Notify,
    /// Set when the adapter is stopping.
    pub stopping: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
    /// Frames for the relay dropped because the stream's control queue was
    /// full (immediate transfer results and directory replies).
    pub dropped: std::sync::atomic::AtomicU64,
    /// Hub directory queries waiting for BBS's answer, by the query ID BBS
    /// was given.
    pub directory: Mutex<HashMap<String, oneshot::Sender<mp::DirectoryResult>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Record claims at the stream boundary. Runs inside a link's queue lock,
/// so it only notes the event (a leaf lock) for [`Shared::apply_boundary`].
/// Holds the adapter weakly, so a transport task that outlives it keeps
/// nothing open; a claim it can no longer record stays attempted and
/// becomes unknown at the next start.
struct Boundary(Weak<Shared>);

impl Boundary {
    fn note(&self, taken: bool, claim: Claim) {
        if let Some(shared) = self.0.upgrade() {
            lock(&shared.boundary).push((taken, claim));
            shared.boundary_kick.notify_one();
        }
    }
}

impl Settle for Boundary {
    fn taken(&self, claim: Claim) {
        self.note(true, claim);
    }

    fn unsent(&self, claim: Claim) {
        self.note(false, claim);
    }
}

/// Cut a diagnostic to Hub's 1024-byte bound, at a character boundary.
pub(crate) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Whether `reason` has a reason code's form: 1-64 of `[a-z0-9_]`. Codes
/// this adapter does not know pass (D108: a newer peer may have more).
fn well_formed(reason: &str) -> bool {
    (1..=64).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A rejection's reason as recorded for BBS: the relay's code, or
/// `rejected` for an empty or malformed one, as Hub records a rejection
/// without a code (internal/fed/transfer.go, onTransferResult).
pub(crate) fn code(reason: &str) -> String {
    if well_formed(reason) {
        reason.to_string()
    } else {
        "rejected".to_string()
    }
}

/// A rejection's reason as sent to the relay, which must be a reason code:
/// BBS's, or `invalid_message` for an empty or malformed one.
pub(crate) fn relay_code(reason: &str) -> String {
    if well_formed(reason) {
        reason.to_string()
    } else {
        verify::INVALID_MESSAGE.to_string()
    }
}

impl Shared {
    pub fn store(&self) -> MutexGuard<'_, Store> {
        debug_assert!(
            !link::holding_queue(),
            "lock order: the store is never taken inside a link's queue lock"
        );
        lock(&self.store)
    }

    /// Record what transports took or returned: taken handoffs are sent,
    /// taken results reported; returned handoffs go back to pending (and
    /// their dispatch is woken). Returned results stay owed (BBS's) or are
    /// dropped with their stream (the relay's).
    pub fn apply_boundary(&self) {
        let events = std::mem::take(&mut *lock(&self.boundary));
        if events.is_empty() {
            return;
        }
        let now = now_micros();
        let store = self.store();
        for (taken, claim) in events {
            let done = match (taken, claim) {
                (true, Claim::Handoff(dir, id)) => store.mark_sent(dir, id, now),
                (true, Claim::Report(id)) => store.mark_reported(id, now),
                (true, Claim::RelayResult(id)) => store.mark_relay_reported(id),
                (true, Claim::RelayDuplicate(id)) => store.mark_duplicate_reported(id),
                (false, Claim::Handoff(dir, id)) => {
                    match dir {
                        Direction::Outbound => self.relay_kick.notify_one(),
                        Direction::Inbound => self.bbs_kick.notify_one(),
                    }
                    store.requeue(dir, id, now)
                }
                (false, Claim::Report(_) | Claim::RelayResult(_) | Claim::RelayDuplicate(_)) => {
                    Ok(())
                }
            };
            if let Err(err) = done {
                tracing::error!(?claim, taken, "recording a stream's handoff: {err:#}");
            }
        }
    }

    /// Ask every task to stop.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub fn is_stopping(&self) -> bool {
        *self.stopping.borrow()
    }

    pub fn settle(self: &Arc<Self>) -> Arc<dyn Settle> {
        Arc::new(Boundary(Arc::downgrade(self)))
    }

    pub fn bbs_link(&self) -> Option<link::Sender<pb::AdapterFrame>> {
        lock(&self.bbs).clone()
    }

    /// Settle work past its deadline, both ways, and report it; then drop
    /// what retention allows.
    pub fn sweep(&self) {
        let now = now_micros();
        let (out, inc, lapsed, purged) = {
            let mut store = self.store();
            let out = store.expire(Direction::Outbound, now);
            let inc = store.expire(Direction::Inbound, now);
            let lapsed = store
                .lapse(Direction::Outbound, now)
                .and_then(|o| Ok((o, store.lapse(Direction::Inbound, now)?)));
            // A retention too long to subtract keeps everything.
            let before = self
                .opts
                .retention
                .and_then(|r| now.checked_sub(i64::try_from(r.as_micros()).ok()?))
                .unwrap_or(i64::MIN);
            let purged = store.purge_settled(before, now);
            (out, inc, lapsed, purged)
        };
        match out {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(
                    expired = ids.len(),
                    "remote sends expired before the relay took them"
                );
                self.report_kick.notify_one();
            }
            Ok(_) => {}
            Err(err) => tracing::error!("expiring outbound work: {err:#}"),
        }
        match inc {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(expired = ids.len(), "imports expired before BBS took them");
                self.relay_report_kick.notify_one();
            }
            Ok(_) => {}
            Err(err) => tracing::error!("expiring inbound work: {err:#}"),
        }
        match lapsed {
            Ok((0, 0)) => {}
            Ok((outbound, inbound)) => tracing::info!(
                outbound,
                inbound,
                "sent handoffs had no result by their deadline; now unknown"
            ),
            Err(err) => tracing::error!("lapsing sent work: {err:#}"),
        }
        match purged {
            Ok((0, 0)) => {}
            Ok((outbound, inbound)) => {
                tracing::info!(outbound, inbound, "dropped settled handoffs past retention")
            }
            Err(err) => tracing::error!("dropping settled handoffs: {err:#}"),
        }
    }

    /// Ask BBS for its answer to a Hub directory query, by `deadline`
    /// (Unix micros). None when BBS does not answer in time: the asker then
    /// times out, rather than mistake silence for an empty directory. With
    /// BBS offline the answer says so.
    pub async fn local_directory(
        &self,
        mut q: mp::DirectoryQuery,
        deadline: i64,
    ) -> Option<mp::DirectoryResult> {
        let failed = |code: &str, detail: &str| mp::DirectoryResult {
            error_code: code.to_string(),
            detail: detail.to_string(),
            ..Default::default()
        };
        let Some(bbs) = self.bbs_link() else {
            return Some(failed(
                verify::HOME_OFFLINE,
                "BBS is not connected to its adapter",
            ));
        };
        let id = crate::new_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = lock(&self.directory);
            if waiting.len() >= MAX_DIRECTORY_QUERIES {
                return Some(failed(
                    verify::QUEUE_FULL,
                    "too many pending directory queries",
                ));
            }
            waiting.insert(id.clone(), tx);
        }
        q.query_id = id.clone();
        let frame = pb::AdapterFrame {
            payload: Some(pb::adapter_frame::Payload::LocalDirectoryQuery(q)),
        };
        if let Err(refused) = bbs.try_send_control(frame) {
            lock(&self.directory).remove(&id);
            return Some(match refused {
                Refused::Full => failed(verify::QUEUE_FULL, "BBS's stream is full"),
                Refused::Closed => failed(verify::HOME_OFFLINE, "BBS's stream has closed"),
            });
        }
        let wait = Duration::from_micros((deadline - now_micros()).max(0) as u64);
        let result = tokio::time::timeout(wait, rx).await;
        lock(&self.directory).remove(&id);
        result.ok()?.ok()
    }

    /// BBS's answer to a directory query this adapter asked.
    pub fn directory_answered(&self, result: mp::DirectoryResult) {
        match lock(&self.directory).remove(&result.query_id) {
            Some(waiter) => {
                let _ = waiter.send(result);
            }
            None => tracing::debug!(
                query = result.query_id,
                "a late or unknown directory result"
            ),
        }
    }
}

fn disposition(o: &Outcome) -> (bool, bool) {
    // (rejected, already known)
    (
        o.state == "rejected",
        o.state == "received" && o.reason == "already_known",
    )
}

/// An outbound outcome as BBS's result frame.
pub(crate) fn handoff_result(o: &Outcome) -> pb::HandoffResult {
    let (rejected, known) = disposition(o);
    let (disposition, reason, detail) = if rejected {
        (
            pb::HandoffDisposition::Rejected,
            code(&o.reason),
            bounded(&o.detail, 1024),
        )
    } else if known {
        (
            pb::HandoffDisposition::AlreadyKnown,
            String::new(),
            String::new(),
        )
    } else {
        (
            pb::HandoffDisposition::Received,
            String::new(),
            String::new(),
        )
    };
    pb::HandoffResult {
        handoff_id: o.handoff_id.clone(),
        disposition: disposition as i32,
        reason,
        detail,
    }
}

/// An inbound outcome as the relay's result frame.
pub(crate) fn transfer_result(o: &Outcome) -> mp::TransferResult {
    let (rejected, known) = disposition(o);
    let (disposition, error_code, detail) = if rejected {
        (
            mp::TransferDisposition::Rejected,
            relay_code(&o.reason),
            bounded(&o.detail, 1024),
        )
    } else if known {
        (
            mp::TransferDisposition::AlreadyKnown,
            String::new(),
            String::new(),
        )
    } else {
        (
            mp::TransferDisposition::Received,
            String::new(),
            String::new(),
        )
    };
    mp::TransferResult {
        handoff_id: o.handoff_id.clone(),
        disposition: disposition as i32,
        error_code,
        detail,
    }
}

/// The adapter, loaded and ready to run.
pub struct Adapter {
    pub(crate) shared: Arc<Shared>,
}

impl Adapter {
    /// Open the adapter's store at `store` for `settings`. Handoffs a
    /// previous run left attempted become unknown.
    pub fn open(settings: Settings, store: &Path, opts: Options) -> Result<Adapter> {
        let now = now_micros();
        let st = Store::open(store, now)?;
        st.check_identity(
            &settings.namespace,
            &settings.installation_id,
            settings.key.verifying_key().as_bytes(),
            &settings.credential,
            now,
        )?;
        let (stop, stopping) = watch::channel(false);
        Ok(Adapter {
            shared: Arc::new(Shared {
                s: settings,
                opts,
                store: Mutex::new(st),
                bbs: Mutex::new(None),
                relay: Mutex::new(None),
                relay_kick: Notify::new(),
                bbs_kick: Notify::new(),
                report_kick: Notify::new(),
                relay_report_kick: Notify::new(),
                boundary: Mutex::new(Vec::new()),
                boundary_kick: Notify::new(),
                stopping,
                stop,
                dropped: std::sync::atomic::AtomicU64::new(0),
                directory: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Open the adapter a configuration file describes.
    pub fn from_config(cfg: &Config) -> Result<Adapter> {
        let settings = Settings::load(cfg).context("loading the adapter's keys and credentials")?;
        let opts = Options {
            retention: cfg.retention(),
            ..Options::default()
        };
        Adapter::open(settings, &cfg.store, opts)
    }

    /// Run until `shutdown` completes, or BBS turns out to be a peer this
    /// adapter cannot serve (another namespace, or protocol), or a task
    /// fails. On return every task has stopped, the relay session has ended
    /// (its queued frames given up to [`Options::drain`] to go), claims
    /// still queued are back to pending, and the store is closed.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let shared = self.shared;
        let boundary = tokio::spawn({
            let shared = shared.clone();
            async move {
                loop {
                    shared.boundary_kick.notified().await;
                    shared.apply_boundary();
                }
            }
        });
        let mut bbs = tokio::spawn(crate::bbs::run(shared.clone()));
        let mut relay = tokio::spawn(crate::relay::run(shared.clone()));
        let sweeper = tokio::spawn({
            let shared = shared.clone();
            async move {
                loop {
                    shared.sweep();
                    tokio::time::sleep(shared.opts.sweep).await;
                }
            }
        });
        let (result, bbs_done, relay_done) = tokio::select! {
            _ = shutdown => (Ok(()), false, false),
            r = &mut bbs => (match r {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("the BBS stream task failed: {e}")),
            }, true, false),
            r = &mut relay => {
                let err = match r {
                    Ok(()) => anyhow::anyhow!("the relay stream task ended"),
                    Err(e) => anyhow::anyhow!("the relay stream task failed: {e}"),
                };
                tracing::error!("{err:#}");
                (Err(err), false, true)
            }
        };
        // The relay session ends itself: it stops new work, lets its
        // transport take what is queued for a bounded while, and records
        // its end. BBS's stream has no such drain: it is stopped at once.
        shared.stop();
        bbs.abort();
        sweeper.abort();
        if !bbs_done {
            let _ = (&mut bbs).await;
        }
        if !relay_done {
            match tokio::time::timeout(shared.opts.drain + Duration::from_secs(5), &mut relay).await
            {
                Ok(Err(e)) if e.is_panic() => {
                    tracing::error!("the relay stream task panicked: {e}")
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!("the relay session did not end in time; stopping it");
                    relay.abort();
                    let _ = relay.await;
                }
            }
        }
        let _ = sweeper.await;
        // Taken out of their slots first: a slot is never held while a
        // queue lock is taken.
        let bbs_link = lock(&shared.bbs).take();
        if let Some(s) = bbs_link {
            s.close();
        }
        let relay_link = lock(&shared.relay).take();
        if let Some(r) = relay_link {
            r.tx.close();
        }
        boundary.abort();
        let _ = boundary.await;
        shared.apply_boundary();
        tracing::info!("adapter stopped");
        result
    }
}
