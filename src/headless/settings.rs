//! `~/.config/bbs/headless.toml`: named harnesses and run defaults. Kept apart
//! from `config.toml`, which older clients parse with `deny_unknown_fields`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::app::home_dir;

pub const DEFAULT_MIN_TURN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MidTurn {
    /// Add mail to the running turn when the harness can; otherwise queue it.
    #[default]
    Steer,
    /// Deliver mail that arrives during a turn as the next turn.
    Queue,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Harness used when `--harness` is not given.
    pub default_harness: Option<String>,
    pub mid_turn: Option<MidTurn>,
    /// Minimum time between the starts of two turns, like "60s" or "5m".
    pub min_turn_interval: Option<String>,
    /// `[harness.<name>]` tables.
    #[serde(default, rename = "harness")]
    pub harnesses: BTreeMap<String, HarnessConfig>,
}

/// One named harness: an ACP agent started as a subprocess.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Program and arguments, for example `["omp", "acp"]`.
    pub command: Vec<String>,
    /// Extra environment for the subprocess.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Opaque model value for the session's `model` config option.
    pub model: Option<String>,
    /// Other ACP session config options, by id.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

pub fn path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/bbs/headless.toml"))
}

impl Settings {
    pub fn load() -> Result<Settings> {
        let path = path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Settings::parse(&text).with_context(|| format!("invalid config in {}", path.display()))
    }

    fn parse(text: &str) -> Result<Settings> {
        Ok(toml::from_str(text)?)
    }

    /// The named harness, or the default one.
    pub fn harness(&self, name: Option<&str>) -> Result<(String, HarnessConfig)> {
        let name = match name.or(self.default_harness.as_deref()) {
            Some(name) => name.to_string(),
            None if self.harnesses.len() == 1 => self.harnesses.keys().next().unwrap().clone(),
            None => bail!(
                "no harness chosen; pass --harness or set default_harness in {}",
                path()?.display()
            ),
        };
        let Some(config) = self.harnesses.get(&name) else {
            bail!(
                "no harness named {name:?}; add [harness.{name}] to {}",
                path()?.display()
            );
        };
        if config.command.is_empty() {
            bail!("harness {name:?} has an empty command");
        }
        Ok((name, config.clone()))
    }

    pub fn min_turn_interval(&self) -> Result<Duration> {
        match &self.min_turn_interval {
            Some(s) => humantime::parse_duration(s)
                .with_context(|| format!("invalid min_turn_interval {s:?}")),
            None => Ok(DEFAULT_MIN_TURN_INTERVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_harness_tables_and_default() {
        let s = Settings::parse(
            r#"
default_harness = "omp"
mid_turn = "queue"
min_turn_interval = "2m"

[harness.omp]
command = ["omp", "acp"]
model = "some/model"
config = { mode = "default" }

[harness.codex]
command = ["npx", "-y", "@zed-industries/codex-acp"]
env = { RUST_LOG = "warn" }
"#,
        )
        .unwrap();
        assert_eq!(s.default_harness.as_deref(), Some("omp"));
        assert_eq!(s.mid_turn, Some(MidTurn::Queue));
        assert_eq!(s.min_turn_interval().unwrap(), Duration::from_secs(120));
        let (name, omp) = s.harness(None).unwrap();
        assert_eq!(name, "omp");
        assert_eq!(omp.command, ["omp", "acp"]);
        assert_eq!(omp.model.as_deref(), Some("some/model"));
        assert_eq!(omp.config["mode"], "default");
        let (_, codex) = s.harness(Some("codex")).unwrap();
        assert_eq!(codex.env["RUST_LOG"], "warn");
        assert!(s.harness(Some("missing")).is_err());
    }

    #[test]
    fn single_harness_is_the_default() {
        let s = Settings::parse("[harness.omp]\ncommand = [\"omp\", \"acp\"]\n").unwrap();
        assert_eq!(s.harness(None).unwrap().0, "omp");
        assert_eq!(s.min_turn_interval().unwrap(), DEFAULT_MIN_TURN_INTERVAL);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Settings::parse("bogus = 1\n").is_err());
        assert!(Settings::parse("[harness.omp]\ncommand = [\"x\"]\nbogus = 1\n").is_err());
    }
}
