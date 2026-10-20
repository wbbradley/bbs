//! Local UI choices, separate from authentication and server configuration.

use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::theme::{self, Scheme};

/// Unknown fields are ignored and dropped on the next save.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Values {
    /// Agent conversations show compact view; absent means verbose. Saved
    /// either way, so an older client keeps verbose too.
    compact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<Scheme>,
}

/// Local appearance choices shared by every screen in this app.
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

    /// CLI, BBS_THEME and NO_COLOR override the saved preference at startup.
    /// A saved choice otherwise takes precedence over the Gruvbox default.
    pub fn restore_theme(&self, explicit: bool) {
        if !explicit && let Some(scheme) = self.values.theme {
            theme::select(scheme);
        }
    }

    pub fn set_theme(&mut self, scheme: Scheme) {
        self.values.theme = Some(scheme);
        // Retain the live choice if persistence fails, and let the screen explain it.
        self.error = self
            .save()
            .err()
            .map(|e| format!("Cannot save color scheme: {e:#}"));
    }

    /// Whether agent conversations show compact view.
    pub fn compact(&self) -> bool {
        self.values.compact
    }

    pub fn set_compact(&mut self, compact: bool) {
        self.values.compact = compact;
        // Keep the live choice if persistence fails, and let the screen explain it.
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
    fn saved_theme_round_trips_and_honors_startup_overrides() {
        use crate::ui::theme::testing::with_scheme;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connect.json");
        // A file with only a view choice loads; theme saves keep it.
        std::fs::write(&path, r#"{"compact":false}"#).unwrap();
        let mut prefs = Preferences::load(path.clone()).unwrap();
        with_scheme(Scheme::Terminal, || {
            prefs.restore_theme(false);
            assert_eq!(theme::current(), Scheme::Terminal);
        });
        for saved in Scheme::ALL {
            prefs.set_theme(saved);
            assert!(prefs.error.is_none());
            prefs = Preferences::load(path.clone()).unwrap();
            with_scheme(Scheme::NoColor, || {
                prefs.restore_theme(true);
                assert_eq!(
                    theme::current(),
                    Scheme::NoColor,
                    "NO_COLOR startup opt-out wins"
                );
                theme::select(Scheme::Gruvbox);
                prefs.restore_theme(false);
                assert_eq!(theme::current(), saved, "saved choice overrides default");
            });
        }
        assert_eq!(prefs.values.theme, Some(Scheme::NoColor));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::json!({"compact": false, "theme": "no-color"})
        );
    }

    #[test]
    fn failed_theme_save_keeps_live_choice_and_can_be_retried() {
        use crate::ui::theme::testing::with_scheme;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("state");
        let path = parent.join("connect.json");
        let mut prefs = Preferences::load(path.clone()).unwrap();
        std::fs::write(&parent, "blocked").unwrap();
        with_scheme(Scheme::Gruvbox, || {
            theme::select(Scheme::Terminal);
            prefs.set_theme(Scheme::Terminal);
            assert_eq!(theme::current(), Scheme::Terminal);
            assert!(
                prefs
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("Cannot save color scheme")
            );
            std::fs::remove_file(&parent).unwrap();
            prefs.set_theme(Scheme::Terminal);
            assert!(prefs.error.is_none());
            assert_eq!(
                Preferences::load(path).unwrap().values.theme,
                Some(Scheme::Terminal)
            );
        });
    }

    #[test]
    fn legacy_compact_and_theme_round_trip_and_each_save_keeps_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connect.json");
        let saved = |path: &std::path::Path| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
        };
        // As older clients wrote it.
        std::fs::write(&path, r#"{"compact":true,"theme":"terminal"}"#).unwrap();
        let mut prefs = Preferences::load(path.clone()).unwrap();
        assert!(prefs.compact());
        prefs.set_theme(Scheme::NoColor);
        assert_eq!(
            saved(&path),
            serde_json::json!({"compact": true, "theme": "no-color"})
        );
        prefs.set_compact(false);
        assert!(prefs.error.is_none());
        assert_eq!(
            saved(&path),
            serde_json::json!({"compact": false, "theme": "no-color"})
        );
        let mut prefs = Preferences::load(path.clone()).unwrap();
        assert!(!prefs.compact());
        assert_eq!(prefs.values.theme, Some(Scheme::NoColor));
        prefs.set_compact(true);
        assert_eq!(
            saved(&path),
            serde_json::json!({"compact": true, "theme": "no-color"})
        );
        assert!(!Preferences::default().compact(), "verbose by default");
    }

    #[test]
    fn missing_preferences_create_no_state_until_a_choice_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/connect.json");
        let mut preferences = Preferences::load(path.clone()).unwrap();
        assert!(!path.exists(), "loading defaults does not create state");
        preferences.set_theme(Scheme::Terminal);
        assert!(preferences.error.is_none());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}
