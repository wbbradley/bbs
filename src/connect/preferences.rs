//! Local UI choices, separate from authentication and server configuration.

use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Values {
    compact: bool,
}

impl Default for Values {
    fn default() -> Self {
        Self { compact: true }
    }
}

/// One preference shared by all agent conversations in this app.
#[derive(Default)]
pub struct Preferences {
    values: Values,
    /// No path is needed for an in-memory app (for example, UI tests).
    path: Option<PathBuf>,
    pub error: Option<String>,
}

impl Preferences {
    pub fn load(path: PathBuf) -> Result<Self> {
        let values = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Values::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            values,
            path: Some(path),
            error: None,
        })
    }

    pub fn compact(&self) -> bool {
        self.values.compact
    }

    pub fn set_compact(&mut self, compact: bool) {
        self.values.compact = compact;
        // Keep the choice for this session even if it cannot be saved.
        self.error = self
            .save()
            .err()
            .map(|e| format!("Cannot save view preference: {e:#}"));
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().context("UI preference path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, &self.values)?;
        file.write_all(b"\n")?;
        file.persist(path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preferences_default_to_compact_and_both_choices_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/connect.json");
        let mut preferences = Preferences::load(path.clone()).unwrap();
        assert!(preferences.compact());
        assert!(!path.exists(), "loading defaults does not create state");
        for compact in [false, true] {
            preferences.set_compact(compact);
            assert!(preferences.error.is_none());
            preferences = Preferences::load(path.clone()).unwrap();
            assert_eq!(preferences.compact(), compact);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap())
                    .unwrap(),
                serde_json::json!({"compact": compact})
            );
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn saving_errors_keep_the_session_choice_and_clear_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("state");
        let mut preferences = Preferences::load(parent.join("connect.json")).unwrap();
        std::fs::write(&parent, "blocked").unwrap();
        preferences.set_compact(false);
        assert!(!preferences.compact());
        assert!(
            preferences
                .error
                .as_deref()
                .unwrap()
                .contains("Cannot save view preference")
        );
        std::fs::remove_file(&parent).unwrap();
        preferences.set_compact(true);
        assert!(preferences.error.is_none());
    }
}
