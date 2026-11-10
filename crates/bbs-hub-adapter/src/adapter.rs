//! The running adapter: its store, keys and the two streams it keeps open,
//! one to BBS's integration listener ([`crate::bbs`]) and one to the Hub
//! relay ([`crate::relay`]). Each side reconnects on its own with jittered
//! exponential backoff; work for a side that is down stays pending in the
//! store.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bbs_core::hub_proto::integration as pb;
use bbs_core::hub_proto::msgproto_v1 as mp;
use ed25519_dalek::SigningKey;
use tokio::sync::{Notify, oneshot};

use crate::config::Config;
use crate::link::{self, Claim, Settle};
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
    /// Hub directory queries waiting for BBS's answer, by the query ID BBS
    /// was given.
    pub directory: Mutex<HashMap<String, oneshot::Sender<mp::DirectoryResult>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Record claims at the stream boundary. Holds the adapter weakly, so a
/// transport task that outlives it keeps nothing open; a claim it can no
/// longer record stays attempted and becomes unknown at the next start.
struct Boundary(Weak<Shared>);

impl Settle for Boundary {
    fn taken(&self, claim: Claim) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        match claim {
            Claim::Handoff(dir, id) => {
                if let Err(err) = shared.store().mark_sent(dir, id, now_micros()) {
                    tracing::error!("recording a handoff as sent: {err:#}");
                }
            }
            Claim::Report(id) => {
                if let Err(err) = shared.store().mark_reported(id) {
                    tracing::error!("recording a result as reported: {err:#}");
                }
            }
        }
    }

    fn unsent(&self, claim: Claim) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        match claim {
            Claim::Handoff(dir, id) => {
                if let Err(err) = shared.store().requeue(dir, id, now_micros()) {
                    tracing::error!("returning an unsent handoff to pending: {err:#}");
                }
                match dir {
                    Direction::Outbound => shared.relay_kick.notify_one(),
                    Direction::Inbound => shared.bbs_kick.notify_one(),
                }
            }
            // Still owed: the next stream to BBS reports it.
            Claim::Report(_) => {}
        }
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

/// A reason code as results carry it: ASCII, at most 64 bytes.
pub(crate) fn code(reason: &str) -> String {
    if !reason.is_empty() && reason.len() <= 64 && reason.bytes().all(|b| b.is_ascii_graphic()) {
        reason.to_string()
    } else {
        "rejected".to_string()
    }
}

impl Shared {
    pub fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    pub fn settle(self: &Arc<Self>) -> Arc<dyn Settle> {
        Arc::new(Boundary(Arc::downgrade(self)))
    }

    pub fn bbs_link(&self) -> Option<link::Sender<pb::AdapterFrame>> {
        lock(&self.bbs).clone()
    }

    pub fn relay_link(&self) -> Option<RelayLink> {
        lock(&self.relay).clone()
    }

    /// Tell the relay BBS's result for an inbound handoff, on the stream it
    /// arrived on only (Hub accepts a result from no other, CONTRACT.md:
    /// TransferResult). So it is never kept to send again on a later stream:
    /// a result that cannot go is informative and dropped.
    pub fn report_inbound(&self, o: &Outcome) {
        let Some(relay) = self.relay_link().filter(|r| r.connection == o.connection) else {
            tracing::info!(
                handoff = o.handoff_id,
                state = o.state,
                "the relay stream an import arrived on has gone; its result is not reported"
            );
            return;
        };
        let frame = mp::FederationFrame {
            payload: Some(mp::federation_frame::Payload::TransferResult(
                transfer_result(o),
            )),
        };
        if !relay.tx.try_send(prost::Message::encode_to_vec(&frame)) {
            tracing::warn!(
                handoff = o.handoff_id,
                "the relay stream is full; an import's result is not reported"
            );
        }
    }

    /// Settle pending work past its deadline, both ways, and report it.
    pub fn sweep(&self) {
        let now = now_micros();
        let (out, inc) = {
            let mut store = self.store();
            (
                store.expire(Direction::Outbound, now),
                store.expire(Direction::Inbound, now),
            )
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
            Ok(ids) => {
                if !ids.is_empty() {
                    tracing::info!(expired = ids.len(), "imports expired before BBS took them");
                }
                for id in ids {
                    if let Ok(Some(o)) = self.store().inbound_outcome_of(id) {
                        self.report_inbound(&o);
                    }
                }
            }
            Err(err) => tracing::error!("expiring inbound work: {err:#}"),
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
        if !bbs.try_send(frame) {
            lock(&self.directory).remove(&id);
            return Some(failed(
                verify::HOME_OFFLINE,
                "BBS's stream is full or closed",
            ));
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
            code(&o.reason),
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
                directory: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Open the adapter a configuration file describes.
    pub fn from_config(cfg: &Config) -> Result<Adapter> {
        let settings = Settings::load(cfg).context("loading the adapter's keys and credentials")?;
        Adapter::open(settings, &cfg.store, Options::default())
    }

    /// Run until `shutdown` completes, or BBS turns out to be a peer this
    /// adapter cannot serve (another namespace, or protocol). On return
    /// every task has stopped and the store is closed; claims still queued
    /// returned to pending.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let shared = self.shared;
        let bbs = tokio::spawn(crate::bbs::run(shared.clone()));
        let relay = tokio::spawn(crate::relay::run(shared.clone()));
        let sweeper = tokio::spawn({
            let shared = shared.clone();
            async move {
                loop {
                    shared.sweep();
                    tokio::time::sleep(shared.opts.sweep).await;
                }
            }
        });
        tokio::pin!(bbs);
        let (result, bbs_done) = tokio::select! {
            _ = shutdown => (Ok(()), false),
            r = &mut bbs => (match r {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("the BBS stream task failed: {e}")),
            }, true),
        };
        bbs.abort();
        relay.abort();
        sweeper.abort();
        if !bbs_done {
            let _ = (&mut bbs).await;
        }
        let _ = relay.await;
        let _ = sweeper.await;
        if let Some(s) = lock(&shared.bbs).take() {
            s.close();
        }
        if let Some(r) = lock(&shared.relay).take() {
            r.tx.close();
        }
        tracing::info!("adapter stopped");
        result
    }
}
