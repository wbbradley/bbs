//! Local draft text, scoped to a board and connected human.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Draft {
    pub text: String,
    pub cursor: (usize, usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
enum Board {
    Remote(String),
    // Raw Unix bytes preserve non-UTF-8 local paths without lossy collisions.
    Local(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct Scope {
    board: Board,
    human: String,
}

#[derive(Deserialize, Serialize)]
struct StoredDraft {
    scope: Scope,
    target: String,
    draft: Draft,
}

#[derive(Default)]
pub struct DraftStore {
    dir: Option<PathBuf>,
    scope: Option<Scope>,
    saved: HashMap<String, Draft>,
    errors: BTreeMap<String, String>,
    /// Never replace a draft we could not read with an empty or partial editor.
    unreadable: HashSet<String>,
}

impl DraftStore {
    pub fn new(root: &Path, server: Option<&str>, board: &Path, human: &str) -> Result<Self> {
        let board = match server {
            Some(server) => Board::Remote(server.to_ascii_lowercase()),
            None => Board::Local(board.canonicalize()?.as_os_str().as_bytes().to_vec()),
        };
        let scope = Scope {
            board,
            human: human.to_ascii_lowercase(),
        };
        let hash = Sha256::digest(serde_json::to_vec(&scope)?);
        Ok(Self {
            dir: Some(root.join(format!("{hash:x}"))),
            scope: Some(scope),
            ..Default::default()
        })
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))))
    }

    pub fn load(&mut self, key: &str) -> Option<Draft> {
        let path = self.path(key)?;
        let result = match std::fs::read(&path) {
            Ok(bytes) => (|| {
                let stored: StoredDraft =
                    serde_json::from_slice(&bytes).context("invalid draft file")?;
                anyhow::ensure!(
                    Some(&stored.scope) == self.scope.as_ref() && stored.target == key,
                    "draft identity does not match this discussion"
                );
                Ok(stored.draft)
            })(),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Draft::default()),
            Err(e) => Err(e.into()),
        };
        match result {
            Ok(draft) => {
                self.saved.insert(key.to_string(), draft.clone());
                Some(draft)
            }
            Err(e) => {
                self.unreadable.insert(key.to_string());
                self.errors.insert(
                    key.to_string(),
                    format!(
                        "Draft load failed; repair file and restart ({key}): {}: {e}",
                        path.display()
                    ),
                );
                None
            }
        }
    }

    pub fn save(&mut self, key: &str, draft: Draft) {
        if self.unreadable.contains(key) {
            return;
        }
        if self.saved.get(key) == Some(&draft) {
            self.errors.remove(key);
            return;
        }
        let Some(path) = self.path(key) else { return };
        let result = self.write(&path, key, &draft);
        match result {
            Ok(()) => {
                self.saved.insert(key.to_string(), draft);
                self.errors.remove(key);
            }
            Err(e) => {
                self.errors.insert(
                    key.to_string(),
                    format!("Draft save failed; retrying ({key}): {e}"),
                );
            }
        }
    }

    fn write(&self, path: &Path, key: &str, draft: &Draft) -> Result<()> {
        if draft.text.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            };
        }
        let parent = path.parent().context("draft path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(
            &mut file,
            &StoredDraft {
                scope: self.scope.clone().expect("disk storage has a scope"),
                target: key.to_string(),
                draft: draft.clone(),
            },
        )?;
        file.write_all(b"\n")?;
        file.persist(path).context("replacing draft file")?;
        Ok(())
    }

    pub fn error(&self) -> Option<&str> {
        self.errors.values().next().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str) -> Draft {
        Draft {
            text: text.into(),
            cursor: (1, 2),
        }
    }

    #[test]
    fn isolates_boards_humans_and_targets_without_path_components() {
        let root = tempfile::tempdir().unwrap();
        let board = tempfile::tempdir().unwrap();
        let other_board = tempfile::tempdir().unwrap();
        let mut store =
            DraftStore::new(root.path(), Some("BBS:7777"), board.path(), "Ada").unwrap();
        store.save("agent:../mateo", draft("private"));
        let path = store.path("agent:../mateo").unwrap();
        assert_eq!(path.parent(), store.dir.as_deref());
        assert!(store.error().is_none());
        let mut same = DraftStore::new(root.path(), Some("bbs:7777"), board.path(), "ada").unwrap();
        assert_eq!(same.load("agent:../mateo"), Some(draft("private")));
        for (server, local, human) in [
            (Some("other:7777"), board.path(), "ada"),
            (Some("bbs:7778"), board.path(), "ada"),
            (Some("bbs:7777"), board.path(), "bob"),
            (None, board.path(), "ada"),
            (None, other_board.path(), "ada"),
        ] {
            let mut other = DraftStore::new(root.path(), server, local, human).unwrap();
            assert_eq!(other.load("agent:../mateo"), Some(Draft::default()));
            assert_ne!(other.dir, store.dir);
        }
        assert_eq!(same.load("group:../mateo"), Some(Draft::default()));
        let local = DraftStore::new(root.path(), None, board.path(), "ada").unwrap();
        let other = DraftStore::new(root.path(), None, other_board.path(), "ada").unwrap();
        assert_ne!(local.dir, other.dir);
        let alias = DraftStore::new(root.path(), None, &board.path().join("."), "ada").unwrap();
        assert_eq!(local.dir, alias.dir);
    }

    #[test]
    fn separate_sessions_preserve_other_discussions_and_empty_removes_file() {
        let root = tempfile::tempdir().unwrap();
        let mut first = DraftStore::new(root.path(), Some("bbs:7777"), root.path(), "ada").unwrap();
        let mut second =
            DraftStore::new(root.path(), Some("bbs:7777"), root.path(), "ada").unwrap();
        first.save("agent:mateo", draft("one\ntwo\n"));
        second.save("group:crew", draft("  "));
        assert_eq!(first.load("group:crew"), Some(draft("  ")));
        assert_eq!(second.load("agent:mateo"), Some(draft("one\ntwo\n")));
        first.save("agent:mateo", Draft::default());
        assert!(!first.path("agent:mateo").unwrap().exists());
        assert!(second.path("group:crew").unwrap().exists());
    }

    #[test]
    fn unchanged_draft_is_not_rewritten_and_failed_write_keeps_previous_data() {
        let root = tempfile::tempdir().unwrap();
        let mut store = DraftStore::new(root.path(), Some("bbs:7777"), root.path(), "ada").unwrap();
        store.save("agent:mateo", draft("first"));
        let path = store.path("agent:mateo").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // Moving the directory makes any attempt to rewrite it fail, without
        // relying on permissions (tests may run as root) or filesystem clocks.
        let dir = store.dir.clone().unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&dir, &moved).unwrap();
        std::fs::write(&dir, "blocked").unwrap();
        store.save("agent:mateo", draft("first"));
        assert!(store.error().is_none(), "unchanged draft did no I/O");
        store.save("agent:mateo", draft("later"));
        assert!(store.error().unwrap().contains("Draft save failed"));
        assert_eq!(
            std::fs::read(moved.join(path.file_name().unwrap())).unwrap(),
            bytes
        );
        std::fs::remove_file(&dir).unwrap();
        std::fs::rename(moved, dir).unwrap();
        store.save("agent:mateo", draft("later"));
        assert!(store.error().is_none());
        assert_eq!(store.load("agent:mateo"), Some(draft("later")));
    }

    #[test]
    fn full_identity_is_checked_before_restore_and_wrong_file_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let mut source =
            DraftStore::new(root.path(), Some("bbs:7777"), root.path(), "ada").unwrap();
        source.save("agent:mateo", draft("private"));
        let bytes = std::fs::read(source.path("agent:mateo").unwrap()).unwrap();
        for (server, human, target) in [
            ("bbs:7778", "ada", "agent:mateo"),
            ("bbs:7777", "bob", "agent:mateo"),
            ("bbs:7777", "ada", "group:mateo"),
        ] {
            let mut wrong = DraftStore::new(root.path(), Some(server), root.path(), human).unwrap();
            let path = wrong.path(target).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(wrong.load(target).is_none());
            assert!(wrong.error().unwrap().contains("identity does not match"));
            wrong.save(target, Draft::default());
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn unreadable_draft_is_reported_and_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let mut store = DraftStore::new(root.path(), Some("bbs:7777"), root.path(), "ada").unwrap();
        let path = store.path("agent:mateo").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "broken json").unwrap();
        assert!(store.load("agent:mateo").is_none());
        assert!(store.error().unwrap().contains("Draft load failed"));
        store.save("agent:mateo", Draft::default());
        store.save("agent:mateo", draft("session-only edit"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken json");
    }
}
