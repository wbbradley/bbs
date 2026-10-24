//! One SQLite file per board, human and history generation. Only the sync
//! feed writes here; a page and the cursor past it commit together.

use std::path::{Path, PathBuf};

use prost::Message as _;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use super::super::launcher::{Rank, folded, rank};
use crate::proto;

const SCHEMA: i64 = 1;
/// Candidate rows ranked between checks for a newer query.
const CHECK_EVERY: usize = 512;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// The server's history no longer matches what is stored.
    Mismatch(Mismatch),
    /// The file was written by something this client does not understand.
    Unknown(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "history cache: {e}"),
            StoreError::Io(e) => write!(f, "history cache: {e}"),
            StoreError::Mismatch(m) => {
                write!(f, "the server's history changed at message {}", m.id)
            }
            StoreError::Unknown(why) => write!(f, "history cache: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// The stored message whose server copy is gone or different.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub id: i64,
}

/// One page of the sync feed.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub messages: Vec<proto::Message>,
    /// For each message, the human's agents (id and name) whose
    /// conversations hold it.
    pub scopes: Vec<(i64, Vec<(i64, String)>)>,
    pub caught_up: bool,
}

/// A stored message matching a search, at its best line.
#[derive(Clone, Debug)]
pub struct Found {
    pub message: proto::Message,
    pub line: usize,
    pub text: String,
    pub indexed: String,
    pub rank: Rank,
}

/// Where stored messages belong.
#[derive(Clone, Copy, Debug)]
pub enum Place<'a> {
    /// Messages in the conversation of the human's agent of this name.
    Agent(&'a str),
    Group(&'a str),
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
    generation: u64,
}

/// The directory holding one board's and one human's history.
pub fn dir_for(root: &Path, board: &str, human: &str) -> PathBuf {
    let key = serde_json::json!([board, human.to_ascii_lowercase()]).to_string();
    root.join(format!("{:x}", Sha256::digest(key.as_bytes())))
}

fn file(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("gen-{generation}.db"))
}

/// The generation the board's sessions currently write.
fn current(dir: &Path) -> Result<u64, StoreError> {
    match std::fs::read_to_string(dir.join("current")) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|_| StoreError::Unknown(format!("bad generation {text:?}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(1),
        Err(e) => Err(e.into()),
    }
}

/// Point sessions at `generation` by replacing the small pointer file,
/// never a database.
fn point(dir: &Path, generation: u64) -> Result<(), StoreError> {
    let temp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(temp.path(), generation.to_string())?;
    temp.persist(dir.join("current")).map_err(|e| e.error)?;
    Ok(())
}

/// What makes a message the same message: never its read receipt.
fn fingerprint(m: &proto::Message) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(&m.id.to_le_bytes());
    field(m.created_at.as_bytes());
    field(&m.from_kind.to_le_bytes());
    field(m.from_name.as_bytes());
    field(m.to_name.as_deref().unwrap_or("\0").as_bytes());
    field(m.to_group.as_deref().unwrap_or("\0").as_bytes());
    field(m.body.as_bytes());
    field(&m.control.unwrap_or(-1).to_le_bytes());
    hash.finalize().to_vec()
}

impl Store {
    /// The current generation's history, created if new.
    pub fn open(dir: &Path) -> Result<Store, StoreError> {
        std::fs::create_dir_all(dir)?;
        let generation = current(dir)?;
        Self::open_at(dir, generation)
    }

    fn open_at(dir: &Path, generation: u64) -> Result<Store, StoreError> {
        let conn = Connection::open(file(dir, generation))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY,
                 to_group TEXT COLLATE NOCASE,
                 folded TEXT NOT NULL,
                 fp BLOB NOT NULL,
                 row BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS messages_group ON messages (to_group, id);
             CREATE TABLE IF NOT EXISTS scopes (
                 message_id INTEGER NOT NULL,
                 agent_id INTEGER NOT NULL,
                 agent_name TEXT NOT NULL COLLATE NOCASE,
                 PRIMARY KEY (message_id, agent_id)
             );
             CREATE INDEX IF NOT EXISTS scopes_agent ON scopes (agent_name, message_id);",
        )?;
        let schema: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()?;
        match schema {
            None => {
                conn.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', ?1)",
                    [SCHEMA],
                )?;
            }
            Some(SCHEMA) => {}
            Some(other) => {
                return Err(StoreError::Unknown(format!("schema {other} is newer")));
            }
        }
        Ok(Store {
            conn,
            dir: dir.to_path_buf(),
            generation,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn meta<T: rusqlite::types::FromSql>(&self, key: &str) -> Result<Option<T>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// The human this history was last authenticated as.
    pub fn identity(&self) -> Result<Option<String>, StoreError> {
        self.meta("identity")
    }

    pub fn set_identity(&self, human: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('identity', ?1)",
            [human],
        )?;
        Ok(())
    }

    /// The newest message the sync feed has delivered and stored.
    pub fn cursor(&self) -> Result<i64, StoreError> {
        Ok(self.meta("cursor")?.unwrap_or(0))
    }

    pub fn caught_up(&self) -> Result<bool, StoreError> {
        Ok(self.meta::<i64>("caught_up")?.unwrap_or(0) != 0)
    }

    /// When the last page was stored, RFC 3339.
    pub fn synced_at(&self) -> Result<Option<String>, StoreError> {
        self.meta("synced_at")
    }

    /// Where the sync feed resumes: just before the last stored message, so
    /// the first page shows the server still has it.
    pub fn resume_from(&self) -> Result<i64, StoreError> {
        Ok(self.meta::<i64>("anchor")?.map_or(0, |id| id - 1))
    }

    /// Store a page and move the cursor past it, all or nothing. The first
    /// page after connecting (`resumed`) must begin with the anchor.
    pub fn apply(&mut self, batch: &Batch, resumed: bool) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let result = apply(&tx, batch, resumed);
        match result {
            Ok(()) => Ok(tx.commit()?),
            // Dropping the transaction rolls it back.
            Err(e) => Err(e),
        }
    }

    /// Start over in a fresh generation, leaving this one as it is. Another
    /// session that already moved on is joined rather than overtaken.
    pub fn next_generation(self) -> Result<Store, StoreError> {
        let identity = self.identity()?;
        let dir = self.dir.clone();
        let on_disk = current(&dir)?;
        let next = if on_disk > self.generation {
            on_disk
        } else {
            self.generation + 1
        };
        drop(self);
        let store = Self::open_at(&dir, next)?;
        point(&dir, next)?;
        if let Some(human) = identity {
            store.set_identity(&human)?;
        }
        Ok(store)
    }

    /// The best `limit` stored messages for `query`, ranked as the launcher
    /// ranks message lines; `None` once `stale` says a newer query came.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        stale: &dyn Fn() -> bool,
    ) -> Result<Option<Vec<Found>>, StoreError> {
        let query = folded(query.trim());
        if query.is_empty() || limit == 0 {
            return Ok(Some(Vec::new()));
        }
        // Every rank tier needs the query's letters in order somewhere.
        let mut pattern = String::from("%");
        for c in query.chars() {
            if matches!(c, '%' | '_' | '\\') {
                pattern.push('\\');
            }
            pattern.push(c);
            pattern.push('%');
        }
        let mut statement = self.conn.prepare_cached(
            "SELECT id, folded FROM messages WHERE folded LIKE ?1 ESCAPE '\\' ORDER BY id DESC",
        )?;
        let mut rows = statement.query([&pattern])?;
        // Ranked on the folded text alone; only the winners are decoded.
        let mut best: Vec<(Rank, i64, usize)> = Vec::new();
        let mut seen = 0usize;
        while let Some(row) = rows.next()? {
            seen += 1;
            if seen.is_multiple_of(CHECK_EVERY) && stale() {
                return Ok(None);
            }
            let id: i64 = row.get(0)?;
            let text: String = row.get(1)?;
            let found = text
                .lines()
                .enumerate()
                .filter_map(|(line, indexed)| rank(&query, indexed).map(|r| (r, line)))
                .min_by_key(|(r, line)| (*r, *line));
            if let Some((r, line)) = found {
                best.push((r, id, line));
                if best.len() >= limit * 4 {
                    trim(&mut best, limit);
                }
            }
        }
        trim(&mut best, limit);
        let mut found = Vec::with_capacity(best.len());
        for (rank, id, line) in best {
            let (row, indexed): (Vec<u8>, String) = self.conn.query_row(
                "SELECT row, folded FROM messages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let message = decode(&row)?;
            found.push(Found {
                text: message
                    .body
                    .lines()
                    .nth(line)
                    .unwrap_or_default()
                    .to_string(),
                indexed: indexed.lines().nth(line).unwrap_or_default().to_string(),
                message,
                line,
                rank,
            });
        }
        Ok(Some(found))
    }

    /// Up to `limit` stored messages of `place`, oldest first: around
    /// `around` when given, else the newest.
    pub fn context(
        &self,
        place: Place,
        around: Option<i64>,
        limit: usize,
    ) -> Result<Vec<proto::Message>, StoreError> {
        let (filter, name) = match place {
            Place::Agent(name) => (
                "id IN (SELECT message_id FROM scopes WHERE agent_name = ?1)",
                name,
            ),
            Place::Group(name) => ("to_group = ?1", name),
        };
        let pivot = around.unwrap_or(i64::MAX);
        let half = if around.is_some() { limit / 2 } else { limit };
        let older = format!(
            "SELECT row FROM messages WHERE {filter} AND id <= ?2 ORDER BY id DESC LIMIT ?3"
        );
        let newer =
            format!("SELECT row FROM messages WHERE {filter} AND id > ?2 ORDER BY id ASC LIMIT ?3");
        let mut rows: Vec<Vec<u8>> = self
            .conn
            .prepare(&older)?
            .query_map(params![name, pivot, half.max(1) as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows.reverse();
        if around.is_some() {
            let room = limit.saturating_sub(rows.len());
            let later: Vec<Vec<u8>> = self
                .conn
                .prepare(&newer)?
                .query_map(params![name, pivot, room as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            rows.extend(later);
        }
        rows.iter().map(|row| decode(row)).collect()
    }
}

fn decode(row: &[u8]) -> Result<proto::Message, StoreError> {
    proto::Message::decode(row).map_err(|e| StoreError::Unknown(e.to_string()))
}

/// Keep the best `limit`, in the launcher's order for message hits: better
/// tiers first, then newer, then the finer rank.
fn trim(best: &mut Vec<(Rank, i64, usize)>, limit: usize) {
    best.sort_by(|a, b| {
        a.0.tier
            .cmp(&b.0.tier)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    best.truncate(limit);
}

fn apply(tx: &Transaction, batch: &Batch, resumed: bool) -> Result<(), StoreError> {
    let get = |key: &str| -> Result<Option<rusqlite::types::Value>, StoreError> {
        Ok(tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    };
    let anchor: Option<i64> = get("anchor")?.and_then(|v| match v {
        rusqlite::types::Value::Integer(i) => Some(i),
        _ => None,
    });
    if resumed && let Some(id) = anchor {
        let stored: Vec<u8> =
            tx.query_row("SELECT fp FROM messages WHERE id = ?1", [id], |r| r.get(0))?;
        match batch.messages.first() {
            Some(m) if m.id == id && fingerprint(m) == stored => {}
            _ => return Err(StoreError::Mismatch(Mismatch { id })),
        }
    }
    let cursor: i64 = tx
        .query_row("SELECT value FROM meta WHERE key = 'cursor'", [], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0);
    for m in &batch.messages {
        let fp = fingerprint(m);
        let old: Option<Vec<u8>> = tx
            .query_row("SELECT fp FROM messages WHERE id = ?1", [m.id], |r| {
                r.get(0)
            })
            .optional()?;
        match old {
            Some(old) if old != fp => return Err(StoreError::Mismatch(Mismatch { id: m.id })),
            // The same message, perhaps with a newer receipt.
            Some(_) => {
                tx.execute(
                    "UPDATE messages SET row = ?2 WHERE id = ?1",
                    params![m.id, m.encode_to_vec()],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO messages (id, to_group, folded, fp, row) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![m.id, m.to_group, folded(&m.body), fp, m.encode_to_vec()],
                )?;
            }
        }
    }
    for (id, agents) in &batch.scopes {
        for (agent_id, name) in agents {
            tx.execute(
                "INSERT OR IGNORE INTO scopes (message_id, agent_id, agent_name) VALUES (?1, ?2, ?3)",
                params![id, agent_id, name],
            )?;
        }
    }
    let set = |key: &str, value: &dyn rusqlite::ToSql| -> Result<(), StoreError> {
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    };
    let last = batch.messages.last();
    // A duplicate or stale page never moves anything back.
    let current = last.is_none_or(|m| m.id >= cursor);
    if let Some(m) = last
        && m.id > cursor
    {
        set("cursor", &m.id)?;
        set("anchor", &m.id)?;
    }
    if current {
        set("caught_up", &i64::from(batch.caught_up))?;
    }
    set("synced_at", &chrono::Utc::now().to_rfc3339())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, body: &str) -> proto::Message {
        proto::Message {
            id,
            created_at: format!("2026-10-23T12:00:{:02}Z", id % 60),
            from_kind: proto::Kind::Agent as i32,
            from_name: "Zed".into(),
            to_name: Some("ada".into()),
            body: body.into(),
            ..Default::default()
        }
    }

    fn batch(rows: &[(i64, &str)], caught_up: bool) -> Batch {
        Batch {
            messages: rows.iter().map(|(id, body)| message(*id, body)).collect(),
            scopes: rows
                .iter()
                .map(|(id, _)| (*id, vec![(7, "Zed".to_string())]))
                .collect(),
            caught_up,
        }
    }

    fn ids(found: &[Found]) -> Vec<i64> {
        found.iter().map(|f| f.message.id).collect()
    }

    #[test]
    fn pages_and_their_cursor_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.set_identity("ada").unwrap();
        assert_eq!(
            (store.cursor().unwrap(), store.resume_from().unwrap()),
            (0, 0)
        );
        store
            .apply(&batch(&[(1, "one"), (2, "two")], false), false)
            .unwrap();
        store.apply(&batch(&[(5, "five")], true), false).unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.cursor().unwrap(), 5);
        assert_eq!(store.resume_from().unwrap(), 4, "re-deliver the anchor");
        assert!(store.caught_up().unwrap());
        assert_eq!(store.identity().unwrap().as_deref(), Some("ada"));
        assert!(store.synced_at().unwrap().is_some());
        let rows = store.context(Place::Agent("zed"), None, 10).unwrap();
        assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 2, 5]);
    }

    #[test]
    fn a_page_that_fails_part_way_stores_nothing_and_keeps_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.apply(&batch(&[(1, "one")], false), false).unwrap();
        // 3 is new, but 1 comes back different: the whole page is refused.
        let error = store
            .apply(&batch(&[(3, "three"), (1, "changed")], false), false)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Mismatch(Mismatch { id: 1 })),
            "{error}"
        );
        assert_eq!(store.cursor().unwrap(), 1);
        assert!(store.context(Place::Agent("zed"), None, 10).unwrap().len() == 1);
    }

    #[test]
    fn duplicate_and_stale_pages_never_move_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .apply(&batch(&[(1, "one"), (2, "two")], false), false)
            .unwrap();
        store.apply(&batch(&[(3, "three")], true), false).unwrap();
        // The first page again, saying more follows: nothing regresses.
        store
            .apply(&batch(&[(1, "one"), (2, "two")], false), false)
            .unwrap();
        assert_eq!(store.cursor().unwrap(), 3);
        assert!(store.caught_up().unwrap());
        // A receipt is not a different message.
        let mut read = message(2, "two");
        read.read_at = Some("2026-10-23T13:00:00Z".into());
        store
            .apply(
                &Batch {
                    messages: vec![read],
                    ..Batch::default()
                },
                false,
            )
            .unwrap();
        let rows = store.context(Place::Agent("Zed"), None, 10).unwrap();
        assert_eq!(rows[1].read_at.as_deref(), Some("2026-10-23T13:00:00Z"));
    }

    #[test]
    fn resuming_needs_the_anchor_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .apply(&batch(&[(1, "one"), (2, "two")], true), false)
            .unwrap();
        // The server still has 2, as it was.
        store
            .apply(&batch(&[(2, "two"), (3, "three")], true), true)
            .unwrap();
        assert_eq!(store.cursor().unwrap(), 3);
        for page in [
            batch(&[(4, "four")], true),
            batch(&[(3, "rewritten")], true),
            batch(&[], true),
        ] {
            let error = store.apply(&page, true).unwrap_err();
            assert!(
                matches!(error, StoreError::Mismatch(Mismatch { id: 3 })),
                "{error}"
            );
            assert_eq!(store.cursor().unwrap(), 3);
        }
        // A fresh store has nothing to check.
        let other = tempfile::tempdir().unwrap();
        let mut fresh = Store::open(other.path()).unwrap();
        fresh.apply(&batch(&[], true), true).unwrap();
    }

    #[test]
    fn a_new_generation_keeps_the_old_history_and_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.set_identity("ada").unwrap();
        store.apply(&batch(&[(1, "old")], true), false).unwrap();
        let mut next = store.next_generation().unwrap();
        assert_eq!(next.generation(), 2);
        assert_eq!(next.cursor().unwrap(), 0);
        assert_eq!(next.identity().unwrap().as_deref(), Some("ada"));
        next.apply(&batch(&[(1, "new")], true), false).unwrap();
        // New sessions open the new one; the old file is untouched.
        assert_eq!(Store::open(dir.path()).unwrap().generation(), 2);
        let old = Store::open_at(dir.path(), 1).unwrap();
        let rows = old.context(Place::Agent("zed"), None, 10).unwrap();
        assert_eq!(rows[0].body, "old");
        // A session still on 1 joins 2 rather than making 3.
        assert_eq!(old.next_generation().unwrap().generation(), 2);
    }

    #[test]
    fn two_sessions_share_one_history_without_losing_ground() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Store::open(dir.path()).unwrap();
        let mut b = Store::open(dir.path()).unwrap();
        a.apply(&batch(&[(1, "one"), (2, "two")], false), false)
            .unwrap();
        b.apply(&batch(&[(1, "one")], false), false).unwrap();
        b.apply(&batch(&[(2, "two"), (3, "three")], true), false)
            .unwrap();
        a.apply(&batch(&[(2, "two")], false), false).unwrap();
        assert_eq!((a.cursor().unwrap(), b.cursor().unwrap()), (3, 3));
    }

    #[test]
    fn search_ranks_like_the_launcher_and_gives_way_to_newer_queries() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .apply(
                &batch(
                    &[
                        (1, "Hello Jefe, how are you?"),
                        (2, "blah blah abcJefedefgh"),
                        (3, "first line\nthen Jefe!"),
                        (4, "j e f e spread out"),
                        (5, "nothing here"),
                    ],
                    true,
                ),
                false,
            )
            .unwrap();
        let never = || false;
        let found = store.search("JEFE", 10, &never).unwrap().unwrap();
        // Whole words, newest first; then partial; then ordered letters.
        assert_eq!(ids(&found), [3, 1, 2, 4]);
        assert_eq!((found[0].line, found[0].text.as_str()), (1, "then Jefe!"));
        assert_eq!(found[0].indexed, "then jefe!");
        assert_eq!(
            ids(&store.search("jefe", 2, &never).unwrap().unwrap()),
            [3, 1]
        );
        assert!(store.search("  ", 10, &never).unwrap().unwrap().is_empty());
        // Wildcards are text.
        store
            .apply(&batch(&[(6, "100% sure_thing")], true), false)
            .unwrap();
        assert_eq!(
            ids(&store.search("0% s", 10, &never).unwrap().unwrap()),
            [6]
        );
        assert!(store.search("x_y", 10, &never).unwrap().unwrap().is_empty());
        // A newer query stops a long one.
        let many: Vec<(i64, String)> = (10..2000).map(|i| (i, format!("jefe {i}"))).collect();
        let rows: Vec<(i64, &str)> = many.iter().map(|(i, b)| (*i, b.as_str())).collect();
        store.apply(&batch(&rows, true), false).unwrap();
        assert!(store.search("jefe", 10, &|| true).unwrap().is_none());
    }

    #[test]
    fn context_holds_a_window_around_a_hit_or_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let rows: Vec<(i64, String)> = (1..=30).map(|i| (i, format!("m{i}"))).collect();
        let rows: Vec<(i64, &str)> = rows.iter().map(|(i, b)| (*i, b.as_str())).collect();
        store.apply(&batch(&rows, true), false).unwrap();
        let mut group = message(40, "to the crew");
        group.to_group = Some("Crew".into());
        store
            .apply(
                &Batch {
                    messages: vec![group],
                    scopes: vec![(40, Vec::new())],
                    caught_up: true,
                },
                false,
            )
            .unwrap();
        let around = store.context(Place::Agent("Zed"), Some(10), 6).unwrap();
        assert_eq!(
            around.iter().map(|m| m.id).collect::<Vec<_>>(),
            [8, 9, 10, 11, 12, 13]
        );
        let newest = store.context(Place::Agent("Zed"), None, 3).unwrap();
        assert_eq!(
            newest.iter().map(|m| m.id).collect::<Vec<_>>(),
            [28, 29, 30]
        );
        let crew = store.context(Place::Group("crew"), None, 10).unwrap();
        assert_eq!(crew.iter().map(|m| m.id).collect::<Vec<_>>(), [40]);
        assert!(
            store
                .context(Place::Agent("Mateo"), None, 10)
                .unwrap()
                .is_empty()
        );
    }

    /// `cargo test --release -- --ignored large_history`: timings and memory
    /// for a large history, printed rather than asserted.
    #[test]
    #[ignore]
    fn large_history_search_timings() {
        fn rss_kib() -> u64 {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
                })
                .unwrap_or(0)
        }
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let words = [
            "deploy", "review", "the", "gateway", "ledger", "redis", "plan", "merge", "CI",
            "blocker", "branch", "tests", "pass", "worktree", "agent", "human", "release",
        ];
        let total = 100_000i64;
        let started = std::time::Instant::now();
        for page in 0..(total / 1000) {
            let rows: Vec<(i64, String)> = (0..1000)
                .map(|i| {
                    let id = page * 1000 + i + 1;
                    let body = (0..60)
                        .map(|w| words[((id * 31 + w * 7) % words.len() as i64) as usize])
                        .collect::<Vec<_>>()
                        .join(" ");
                    (id, format!("{body} run {id}"))
                })
                .collect();
            let rows: Vec<(i64, &str)> = rows.iter().map(|(i, b)| (*i, b.as_str())).collect();
            store.apply(&batch(&rows, false), false).unwrap();
        }
        let size = std::fs::metadata(file(dir.path(), 1)).unwrap().len();
        println!(
            "stored {total} messages in {:?}, {} MiB on disk",
            started.elapsed(),
            size / (1 << 20)
        );
        let before = rss_kib();
        for query in ["run 99999", "gateway ledger", "zzz", "dplyrv", "e"] {
            let started = std::time::Instant::now();
            let found = store.search(query, 50, &|| false).unwrap().unwrap();
            println!(
                "{query:>16}: {:>3} hits in {:?}",
                found.len(),
                started.elapsed()
            );
        }
        println!(
            "RSS {} KiB before searching, {} KiB after",
            before,
            rss_kib()
        );
    }
}
