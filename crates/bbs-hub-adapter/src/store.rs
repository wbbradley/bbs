//! The adapter's own SQLite store, separate from BBS's database: this
//! installation's identity, trust (namespace bindings, verified credentials
//! and revocations), the last accepted publication catalog, the relay
//! session, and both directions' handoffs with their exact bytes.
//!
//! Handoffs follow D072: work is claimed (`pending -> attempted`, committed)
//! before it is queued for a stream, becomes `sent` when the transport takes
//! its frame, and `received`/`rejected` on an informative result. A claim
//! still queued when its stream closes provably never left and returns to
//! `pending`; everything else attempted becomes `unknown` at the next start
//! and is never offered again. Settled rows never change, and nothing
//! returns to pending once sent (triggers).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bbs_core::hub_proto::msgproto_v1 as mp;
use prost::Message as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::verify::Credential;

const SCHEMA: &str = r#"
CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    namespace TEXT NOT NULL,
    installation_id TEXT NOT NULL,
    public_key BLOB NOT NULL,
    credential BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE bindings (
    namespace TEXT PRIMARY KEY,
    authority_key BLOB NOT NULL,
    revision INTEGER NOT NULL,
    disabled INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE credentials (
    record_hash BLOB PRIMARY KEY,
    namespace TEXT NOT NULL,
    installation_id TEXT NOT NULL,
    public_key BLOB NOT NULL,
    assert_senders INTEGER NOT NULL,
    host_namespace INTEGER NOT NULL,
    record BLOB NOT NULL,
    first_seen INTEGER NOT NULL
);
CREATE INDEX credentials_namespace ON credentials (namespace);
CREATE TABLE revocations (
    namespace TEXT NOT NULL,
    public_key BLOB NOT NULL,
    record BLOB NOT NULL,
    learned_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, public_key)
);
CREATE TABLE catalog (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL,
    entities INTEGER NOT NULL,
    snapshot BLOB NOT NULL,
    received_at INTEGER NOT NULL
);
CREATE TABLE relay_session (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    connection_id TEXT NOT NULL,
    relay_installation TEXT NOT NULL,
    connected_at INTEGER NOT NULL,
    ready_at INTEGER,
    ended_at INTEGER
);
CREATE TABLE outbound (
    id INTEGER PRIMARY KEY,
    bbs_handoff_id TEXT NOT NULL UNIQUE,
    handoff_id TEXT NOT NULL UNIQUE,
    origin_namespace TEXT NOT NULL,
    origin_message_id TEXT NOT NULL,
    recipient_namespace TEXT NOT NULL,
    message BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    envelope BLOB,
    state TEXT NOT NULL CHECK (state IN
        ('pending', 'attempted', 'sent', 'received', 'rejected', 'unknown')),
    connection_id TEXT,
    reason TEXT,
    detail TEXT,
    created_at INTEGER NOT NULL,
    attempted_at INTEGER,
    updated_at INTEGER NOT NULL,
    UNIQUE (origin_namespace, origin_message_id)
);
CREATE INDEX outbound_pending ON outbound (expires_at, id) WHERE state = 'pending';
CREATE INDEX outbound_attempted ON outbound (id) WHERE state = 'attempted';
-- Results owed to BBS for its handoffs, until the transport takes them.
CREATE TABLE reports (
    id INTEGER PRIMARY KEY,
    bbs_handoff_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('received', 'rejected')),
    reason TEXT NOT NULL,
    detail TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE inbound (
    id INTEGER PRIMARY KEY,
    handoff_id TEXT NOT NULL UNIQUE,
    relay_handoff_id TEXT NOT NULL,
    relay_connection TEXT NOT NULL,
    origin_namespace TEXT NOT NULL,
    origin_message_id TEXT NOT NULL,
    message BLOB NOT NULL,
    envelope BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('pending', 'attempted', 'sent', 'received', 'rejected', 'unknown')),
    reason TEXT,
    detail TEXT,
    created_at INTEGER NOT NULL,
    attempted_at INTEGER,
    updated_at INTEGER NOT NULL,
    UNIQUE (origin_namespace, origin_message_id)
);
CREATE INDEX inbound_pending ON inbound (expires_at, id) WHERE state = 'pending';
CREATE INDEX inbound_attempted ON inbound (id) WHERE state = 'attempted';
CREATE TRIGGER outbound_settled BEFORE UPDATE OF state ON outbound
WHEN OLD.state IN ('received', 'rejected')
    OR (OLD.state IN ('sent', 'unknown') AND NEW.state IN ('pending', 'attempted'))
BEGIN SELECT RAISE(ABORT, 'a handoff never returns to pending once sent, nor changes once settled'); END;
CREATE TRIGGER inbound_settled BEFORE UPDATE OF state ON inbound
WHEN OLD.state IN ('received', 'rejected')
    OR (OLD.state IN ('sent', 'unknown') AND NEW.state IN ('pending', 'attempted'))
BEGIN SELECT RAISE(ABORT, 'a handoff never returns to pending once sent, nor changes once settled'); END;
"#;

const VERSION: i64 = 1;

/// Owe BBS an outbound row's result (`?2` is now); the caller adds the row
/// filter.
const OWE_REPORT: &str = "INSERT INTO reports (bbs_handoff_id, state, reason, detail, created_at)
     SELECT bbs_handoff_id, state, coalesce(reason, ''), coalesce(detail, ''), ?2 FROM outbound";

/// Refuse a secret or private file that others may read or write.
pub fn check_private(path: &Path) -> Result<()> {
    let mode = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is accessible to other users (mode {:o}); restrict it with chmod 600",
            path.display(),
            mode & 0o777
        );
    }
    Ok(())
}

/// A namespace's trusted binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub namespace: String,
    pub authority_key: Vec<u8>,
    pub revision: u64,
    pub disabled: bool,
}

/// What storing a new handoff found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intake {
    New,
    /// The same origin with identical bytes is already known.
    Duplicate,
    /// The same origin with different bytes.
    Conflict,
}

/// A claimed handoff to queue: its row, handoff ID and the bytes to send
/// (an outbound envelope, or an inbound message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: i64,
    pub handoff_id: String,
    pub bytes: Vec<u8>,
}

/// A handoff's settled result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The inbound row, or for a result owed to BBS, its report.
    pub id: i64,
    /// The handoff ID the other side knows it by: BBS's for outbound work,
    /// the relay's for inbound.
    pub handoff_id: String,
    /// For inbound work, the relay stream it arrived on.
    pub connection: String,
    pub state: String,
    pub reason: String,
    pub detail: String,
}

/// Which direction a handoff goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// BBS to Hub.
    Outbound,
    /// Hub to BBS.
    Inbound,
}

impl Direction {
    fn table(self) -> &'static str {
        match self {
            Direction::Outbound => "outbound",
            Direction::Inbound => "inbound",
        }
    }
}

pub struct Store {
    conn: Connection,
    _lock: Option<File>,
}

impl Store {
    /// Open (creating, owner-only) the store at `path` for the one adapter
    /// that may use it, and turn handoffs a previous run left attempted
    /// into unknown.
    pub fn open(path: &Path, now: i64) -> Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
        check_private(path)?;
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if lock.try_lock().is_err() {
            bail!(
                "another adapter is using the store {} (locked by {})",
                path.display(),
                lock_path.display()
            );
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store {
            conn,
            _lock: Some(lock),
        };
        store.migrate()?;
        let (out, inc) = store.recover(now)?;
        if out + inc > 0 {
            tracing::warn!(
                outbound = out,
                inbound = inc,
                "handoffs interrupted by the last stop are now unknown and will not be sent again"
            );
        }
        Ok(store)
    }

    /// Open an existing store read-only, for inspection beside a running
    /// adapter.
    pub fn open_read_only(path: &Path) -> Result<Store> {
        check_private(path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Store { conn, _lock: None })
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                let tx = self.conn.transaction()?;
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", VERSION)?;
                tx.commit()?;
                Ok(())
            }
            VERSION => Ok(()),
            v => bail!("the adapter store has schema {v}; this adapter knows {VERSION}"),
        }
    }

    /// Handoffs left attempted by a stop may or may not have left: unknown,
    /// never sent again.
    fn recover(&mut self, now: i64) -> Result<(usize, usize)> {
        let mut n = [0; 2];
        for (i, dir) in [Direction::Outbound, Direction::Inbound].iter().enumerate() {
            n[i] = self.conn.execute(
                &format!(
                    "UPDATE {} SET state = 'unknown', reason = 'restart_after_attempt',
                     updated_at = ?1 WHERE state = 'attempted'",
                    dir.table()
                ),
                [now],
            )?;
        }
        Ok((n[0], n[1]))
    }

    // ---- identity ----------------------------------------------------------

    /// Record this installation's identity, refusing a store made for
    /// another namespace or installation. A new key and credential for the
    /// same installation replace the old.
    pub fn check_identity(
        &self,
        namespace: &str,
        installation_id: &str,
        public_key: &[u8],
        credential: &mp::SignedRecord,
        now: i64,
    ) -> Result<()> {
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT namespace, installation_id FROM identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((ns, inst)) = existing
            && (ns != namespace || inst != installation_id)
        {
            bail!(
                "this store belongs to installation {inst} of namespace {ns}, not \
                 {installation_id} of {namespace}; use a new store for another installation"
            );
        }
        self.conn.execute(
            "INSERT INTO identity (id, namespace, installation_id, public_key, credential, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET public_key = ?3, credential = ?4, updated_at = ?5
             WHERE public_key != ?3 OR credential != ?4",
            params![
                namespace,
                installation_id,
                public_key,
                credential.encode_to_vec(),
                now
            ],
        )?;
        Ok(())
    }

    // ---- trust ---------------------------------------------------------------

    pub fn binding(&self, namespace: &str) -> Result<Option<Binding>> {
        Ok(self
            .conn
            .query_row(
                "SELECT authority_key, revision, disabled FROM bindings WHERE namespace = ?1",
                [namespace],
                |r| {
                    Ok(Binding {
                        namespace: namespace.to_string(),
                        authority_key: r.get(0)?,
                        revision: r.get::<_, i64>(1)? as u64,
                        disabled: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Store a binding from the trust source: a higher revision replaces
    /// the stored one, a lower one is ignored, and the same revision with
    /// different contents is a conflict that keeps the stored one. Returns
    /// whether it changed anything.
    pub fn put_binding(&self, b: &Binding, now: i64) -> Result<bool> {
        if let Some(cur) = self.binding(&b.namespace)? {
            if b.revision < cur.revision || *b == cur {
                return Ok(false);
            }
            if b.revision == cur.revision {
                bail!(
                    "binding for {} at revision {} conflicts with the stored one",
                    b.namespace,
                    b.revision
                );
            }
        }
        self.conn.execute(
            "INSERT INTO bindings (namespace, authority_key, revision, disabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (namespace) DO UPDATE SET authority_key = ?2, revision = ?3,
                 disabled = ?4, updated_at = ?5",
            params![
                b.namespace,
                b.authority_key,
                b.revision as i64,
                b.disabled,
                now
            ],
        )?;
        Ok(true)
    }

    /// Whether `namespace` has a trusted, enabled binding.
    pub fn enrolled(&self, namespace: &str) -> Result<bool> {
        Ok(self.binding(namespace)?.is_some_and(|b| !b.disabled))
    }

    pub fn put_credential(&self, rec: &mp::SignedRecord, c: &Credential) -> Result<()> {
        let hash = crate::sign::hash(rec).context("credential of unknown kind")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO credentials (record_hash, namespace, installation_id,
                 public_key, assert_senders, host_namespace, record, first_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                hash.as_slice(),
                c.namespace,
                c.installation_id,
                c.public_key,
                c.assert_senders,
                c.host_namespace,
                rec.encode_to_vec(),
                crate::now_micros()
            ],
        )?;
        Ok(())
    }

    pub fn revoked(&self, namespace: &str, public_key: &[u8]) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM revocations WHERE namespace = ?1 AND public_key = ?2",
                params![namespace, public_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Store a verified revocation; returns whether it was new. Revocation
    /// is permanent: an older credential never undoes it.
    pub fn put_revocation(
        &self,
        namespace: &str,
        public_key: &[u8],
        rec: &mp::SignedRecord,
        now: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO revocations (namespace, public_key, record, learned_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![namespace, public_key, rec.encode_to_vec(), now],
        )? > 0)
    }

    // ---- publication and session -------------------------------------------

    /// The last accepted catalog revision.
    pub fn catalog_revision(&self) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row("SELECT revision FROM catalog WHERE id = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .map(|r| r as u64))
    }

    /// Keep a newer catalog snapshot; returns whether it was newer.
    pub fn put_catalog(
        &self,
        catalog: &bbs_core::hub_proto::integration::PublishedCatalog,
        now: i64,
    ) -> Result<bool> {
        if self
            .catalog_revision()?
            .is_some_and(|r| catalog.revision <= r)
        {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO catalog (id, revision, entities, snapshot, received_at)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET revision = ?1, entities = ?2, snapshot = ?3,
                 received_at = ?4",
            params![
                catalog.revision as i64,
                catalog.entities.len() as i64,
                catalog.encode_to_vec(),
                now
            ],
        )?;
        Ok(true)
    }

    pub fn session_started(&self, connection: &str, relay: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO relay_session (id, connection_id, relay_installation, connected_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET connection_id = ?1, relay_installation = ?2,
                 connected_at = ?3, ready_at = NULL, ended_at = NULL",
            params![connection, relay, now],
        )?;
        Ok(())
    }

    pub fn session_ready(&self, connection: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE relay_session SET ready_at = ?2 WHERE connection_id = ?1",
            params![connection, now],
        )?;
        Ok(())
    }

    pub fn session_ended(&self, connection: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE relay_session SET ended_at = ?2 WHERE connection_id = ?1",
            params![connection, now],
        )?;
        Ok(())
    }

    // ---- handoffs ------------------------------------------------------------

    /// Store a message BBS handed off for Hub, pending. A known origin is a
    /// duplicate when its bytes match, else a conflict; either way nothing
    /// new is stored.
    #[allow(clippy::too_many_arguments)]
    pub fn intake_outbound(
        &self,
        bbs_handoff_id: &str,
        handoff_id: &str,
        origin: &mp::MessageRef,
        recipient_namespace: &str,
        message: &[u8],
        expires_at: i64,
        now: i64,
    ) -> Result<Intake> {
        if let Some(intake) = self.known(Direction::Outbound, origin, message)? {
            return Ok(intake);
        }
        if self
            .conn
            .query_row(
                "SELECT 1 FROM outbound WHERE bbs_handoff_id = ?1",
                [bbs_handoff_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Ok(Intake::Conflict);
        }
        self.conn.execute(
            "INSERT INTO outbound (bbs_handoff_id, handoff_id, origin_namespace, origin_message_id,
                 recipient_namespace, message, expires_at, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?8)",
            params![
                bbs_handoff_id,
                handoff_id,
                origin.namespace,
                origin.message_id,
                recipient_namespace,
                message,
                expires_at,
                now
            ],
        )?;
        Ok(Intake::New)
    }

    /// Store a verified message from the relay for BBS, pending, as
    /// [`Store::intake_outbound`] does.
    #[allow(clippy::too_many_arguments)]
    pub fn intake_inbound(
        &self,
        handoff_id: &str,
        relay_handoff_id: &str,
        relay_connection: &str,
        origin: &mp::MessageRef,
        message: &[u8],
        envelope: &[u8],
        expires_at: i64,
        now: i64,
    ) -> Result<Intake> {
        if let Some(intake) = self.known(Direction::Inbound, origin, message)? {
            return Ok(intake);
        }
        self.conn.execute(
            "INSERT INTO inbound (handoff_id, relay_handoff_id, relay_connection,
                 origin_namespace, origin_message_id, message, envelope, expires_at, state,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?9)",
            params![
                handoff_id,
                relay_handoff_id,
                relay_connection,
                origin.namespace,
                origin.message_id,
                message,
                envelope,
                expires_at,
                now
            ],
        )?;
        Ok(Intake::New)
    }

    fn known(
        &self,
        dir: Direction,
        origin: &mp::MessageRef,
        message: &[u8],
    ) -> Result<Option<Intake>> {
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row(
                &format!(
                    "SELECT message FROM {} WHERE origin_namespace = ?1 AND origin_message_id = ?2",
                    dir.table()
                ),
                params![origin.namespace, origin.message_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.map(|s| {
            if s == message {
                Intake::Duplicate
            } else {
                Intake::Conflict
            }
        }))
    }

    /// Reject pending work whose deadline has passed by `now`, as expired.
    /// Returns the rows it rejected.
    pub fn expire(&mut self, dir: Direction, now: i64) -> Result<Vec<i64>> {
        let tx = self.conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id FROM {} WHERE state = 'pending' AND expires_at <= ?1 ORDER BY id",
                dir.table()
            ))?;
            stmt.query_map([now], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?
        };
        for id in &ids {
            tx.execute(
                &format!(
                    "UPDATE {} SET state = 'rejected', reason = 'expired',
                     detail = 'the delivery deadline passed before it was sent', updated_at = ?2
                     WHERE id = ?1 AND state = 'pending'",
                    dir.table()
                ),
                params![id, now],
            )?;
            if dir == Direction::Outbound {
                tx.execute(&format!("{OWE_REPORT} WHERE id = ?1"), params![id, now])?;
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Claim the oldest unexpired pending outbound handoff for relay stream
    /// `connection`: mark it attempted, and commit, before the caller queues
    /// it. Its envelope is signed by `seal` at the first claim and kept
    /// (D114): a claim returned to pending is offered again unchanged.
    pub fn claim_outbound(
        &mut self,
        connection: &str,
        now: i64,
        seal: impl FnOnce(&[u8]) -> Vec<u8>,
    ) -> Result<Option<Claimed>> {
        let tx = self.conn.transaction()?;
        type Row = (i64, String, Vec<u8>, Option<Vec<u8>>);
        let row: Option<Row> = tx
            .query_row(
                "SELECT id, handoff_id, message, envelope FROM outbound
                 WHERE state = 'pending' AND expires_at > ?1 ORDER BY id LIMIT 1",
                [now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, handoff_id, message, envelope)) = row else {
            return Ok(None);
        };
        let envelope = envelope.unwrap_or_else(|| seal(&message));
        tx.execute(
            "UPDATE outbound SET state = 'attempted', envelope = ?2, connection_id = ?3,
                 attempted_at = ?4, updated_at = ?4
             WHERE id = ?1 AND state = 'pending'",
            params![id, envelope, connection, now],
        )?;
        tx.commit()?;
        Ok(Some(Claimed {
            id,
            handoff_id,
            bytes: envelope,
        }))
    }

    /// Claim the oldest unexpired pending inbound handoff for BBS.
    pub fn claim_inbound(&mut self, now: i64) -> Result<Option<Claimed>> {
        let tx = self.conn.transaction()?;
        let row: Option<(i64, String, Vec<u8>)> = tx
            .query_row(
                "SELECT id, handoff_id, message FROM inbound
                 WHERE state = 'pending' AND expires_at > ?1 ORDER BY id LIMIT 1",
                [now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((id, handoff_id, message)) = row else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE inbound SET state = 'attempted', attempted_at = ?2, updated_at = ?2
             WHERE id = ?1 AND state = 'pending'",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(Some(Claimed {
            id,
            handoff_id,
            bytes: message,
        }))
    }

    /// A claim the transport took: `sent`. Never replaces a result already
    /// recorded.
    pub fn mark_sent(&self, dir: Direction, id: i64, now: i64) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE {} SET state = 'sent', updated_at = ?2 WHERE id = ?1 AND state = 'attempted'",
                dir.table()
            ),
            params![id, now],
        )?;
        Ok(())
    }

    /// Claims whose frames provably never left (still queued when their
    /// stream closed) return to pending, to be offered again.
    pub fn requeue(&self, dir: Direction, id: i64, now: i64) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE {} SET state = 'pending', updated_at = ?2 WHERE id = ?1 AND state = 'attempted'",
                dir.table()
            ),
            params![id, now],
        )?;
        Ok(())
    }

    /// Record the relay's result for outbound handoff `handoff_id`, accepted
    /// only from the stream it was attempted on, and only over attempted,
    /// sent or unknown: the first result stands. Returns whether it changed
    /// the handoff (and so has a result to report to BBS).
    pub fn outbound_result(
        &self,
        handoff_id: &str,
        connection: &str,
        state: &str,
        reason: &str,
        detail: &str,
        now: i64,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE outbound SET state = ?3, reason = NULLIF(?4, ''), detail = NULLIF(?5, ''),
                 updated_at = ?6
             WHERE handoff_id = ?1 AND connection_id = ?2
                 AND state IN ('attempted', 'sent', 'unknown')",
            params![handoff_id, connection, state, reason, detail, now],
        )? > 0;
        if changed {
            tx.execute(
                &format!("{OWE_REPORT} WHERE handoff_id = ?1"),
                params![handoff_id, now],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Record BBS's import result for inbound handoff `handoff_id`, over
    /// attempted, sent or unknown only. Returns the outcome to report to the
    /// relay when it changed the handoff.
    pub fn inbound_result(
        &self,
        handoff_id: &str,
        state: &str,
        reason: &str,
        detail: &str,
        now: i64,
    ) -> Result<Option<Outcome>> {
        let changed = self.conn.execute(
            "UPDATE inbound SET state = ?2, reason = NULLIF(?3, ''), detail = NULLIF(?4, ''),
                 updated_at = ?5
             WHERE handoff_id = ?1 AND state IN ('attempted', 'sent', 'unknown')",
            params![handoff_id, state, reason, detail, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.inbound_outcome("handoff_id = ?1", handoff_id)
    }

    fn inbound_outcome(&self, filter: &str, key: &str) -> Result<Option<Outcome>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT id, relay_handoff_id, relay_connection, state,
                         coalesce(reason, ''), coalesce(detail, '')
                     FROM inbound WHERE {filter}"
                ),
                [key],
                |r| {
                    Ok(Outcome {
                        id: r.get(0)?,
                        handoff_id: r.get(1)?,
                        connection: r.get(2)?,
                        state: r.get(3)?,
                        reason: r.get(4)?,
                        detail: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Inbound row `id`'s outcome, to report to the relay.
    pub fn inbound_outcome_of(&self, id: i64) -> Result<Option<Outcome>> {
        self.inbound_outcome("id = ?1", &id.to_string())
    }

    /// Owe BBS a result for its handoff `bbs_handoff_id` that the adapter
    /// settled itself (refused, or already known): kept until the transport
    /// takes it, as a relay's result is.
    pub fn owe_report(
        &self,
        bbs_handoff_id: &str,
        state: &str,
        reason: &str,
        detail: &str,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO reports (bbs_handoff_id, state, reason, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![bbs_handoff_id, state, reason, detail, now],
        )?;
        Ok(())
    }

    /// Results owed to BBS after report `after`, oldest first; each
    /// Outcome's ID is its report's.
    pub fn unreported(&self, after: i64, limit: usize) -> Result<Vec<Outcome>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, bbs_handoff_id, '', state, reason, detail
             FROM reports WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, limit as i64], |r| {
            Ok(Outcome {
                id: r.get(0)?,
                handoff_id: r.get(1)?,
                connection: r.get(2)?,
                state: r.get(3)?,
                reason: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The transport took report `id`: it is owed no longer.
    pub fn mark_reported(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM reports WHERE id = ?1", [id])?;
        Ok(())
    }

    /// A handoff's state, reason and connection (tests and status).
    pub fn state(&self, dir: Direction, id: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT state, coalesce(reason, '') FROM {} WHERE id = ?1",
                    dir.table()
                ),
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Handoffs by state, per direction.
    pub fn counts(&self, dir: Direction) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT state, count(*) FROM {} GROUP BY state ORDER BY state",
            dir.table()
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Trusted namespaces, for status.
    pub fn bindings(&self) -> Result<Vec<Binding>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, authority_key, revision, disabled FROM bindings ORDER BY namespace",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Binding {
                namespace: r.get(0)?,
                authority_key: r.get(1)?,
                revision: r.get::<_, i64>(2)? as u64,
                disabled: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The last relay session: connection ID, relay installation, whether its
    /// route was accepted and whether it has ended.
    pub fn session(&self) -> Result<Option<(String, String, bool, bool)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT connection_id, relay_installation, ready_at IS NOT NULL,
                     ended_at IS NOT NULL FROM relay_session WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests;
