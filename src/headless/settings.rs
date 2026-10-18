//! The `[headless]` table of `config.toml`: named harnesses and run defaults.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::app::home_dir;
use crate::config::{self, Config};

pub const DEFAULT_MIN_TURN_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_TURN_ERRORS: NonZeroU32 = NonZeroU32::new(3).unwrap();

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MidTurn {
    /// Add mail to the running turn when the harness can; otherwise queue it.
    #[default]
    Steer,
    /// Deliver mail that arrives during a turn as the next turn.
    Queue,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Harness used when `--harness` is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_turn: Option<MidTurn>,
    /// Minimum time between the starts of two turns, like "60s" or "5m".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_turn_interval: Option<String>,
    /// Turns that may fail in a row before `run` stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turn_errors: Option<NonZeroU32>,
    /// `[headless.harness.<name>]` tables.
    #[serde(
        default,
        rename = "harness",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub harnesses: BTreeMap<String, HarnessConfig>,
}

/// How `run` talks to a harness.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessType {
    /// An Agent Client Protocol agent.
    #[default]
    Acp,
    /// Codex's app-server, which can steer a running turn.
    CodexAppServer,
}

impl HarnessType {
    fn is_default(&self) -> bool {
        *self == HarnessType::default()
    }
}

/// Where an ACP harness receives the BBS MCP server's environment.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpEnv {
    #[default]
    Server,
    Process,
}

impl McpEnv {
    fn is_default(&self) -> bool {
        *self == Self::Server
    }
}

/// One named harness, started as a subprocess.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    #[serde(
        default,
        rename = "type",
        skip_serializing_if = "HarnessType::is_default"
    )]
    pub kind: HarnessType,
    /// Program and arguments, for example `["omp", "acp"]`. Codex defaults to
    /// `["codex", "app-server"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Extra environment for the subprocess.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// ACP only: use the harness process environment instead of the MCP spec.
    #[serde(default, skip_serializing_if = "McpEnv::is_default")]
    pub mcp_env: McpEnv,
    /// Opaque model value for the session's `model` config option.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Codex only: the model provider, from Codex's own configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Other ACP session config options, by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /// Send the agent's activity (its log records) to the server, where every
    /// human can see it. On unless set to false; see `shares_activity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_activity: Option<bool>,
}

impl HarnessConfig {
    pub fn shares_activity(&self) -> bool {
        self.share_activity.unwrap_or(true)
    }

    /// Check before registration so an incompatible launch cannot take over an agent.
    pub fn check_mcp_env(&self, inherited_scrub: Option<&str>) -> Result<()> {
        if self.mcp_env != McpEnv::Process {
            return Ok(());
        }
        if self.kind != HarnessType::Acp {
            bail!("mcp_env = \"process\" is only supported for ACP harnesses");
        }
        let scrub = self
            .env
            .get("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB")
            .map(String::as_str)
            .or(inherited_scrub);
        if scrub.is_some_and(|s| {
            !matches!(
                s.to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        }) {
            bail!(
                "mcp_env = \"process\" is incompatible with CLAUDE_CODE_SUBPROCESS_ENV_SCRUB: \
                 credential scrubbing can remove the BBS token from the MCP child"
            );
        }
        Ok(())
    }
}

impl Settings {
    /// The `[headless]` table of `config.toml`. Refuses to run while the old
    /// `headless.toml` exists, so its settings are not silently ignored.
    pub fn load() -> Result<Settings> {
        let old = home_dir()?.join(".config/bbs/headless.toml");
        if old.exists() {
            bail!(
                "{} is no longer read; move its contents under [headless] in {} \
                 (harness tables become [headless.harness.<name>]), then delete it",
                old.display(),
                config::path()?.display()
            );
        }
        Ok(Config::read()?.headless.unwrap_or_default())
    }

    /// The named harness, or the default one.
    pub fn harness(&self, name: Option<&str>) -> Result<(String, HarnessConfig)> {
        let name = match name.or(self.default_harness.as_deref()) {
            Some(name) => name.to_string(),
            None if self.harnesses.len() == 1 => self.harnesses.keys().next().unwrap().clone(),
            None => bail!(
                "no harness chosen; pass --harness or set default_harness under [headless] in {}",
                config::path()?.display()
            ),
        };
        let Some(mut config) = self.harnesses.get(&name).cloned() else {
            bail!(
                "no harness named {name:?}; add [headless.harness.{name}] to {}",
                config::path()?.display()
            );
        };
        match config.kind {
            HarnessType::Acp => {
                if config.provider.is_some() {
                    bail!("harness {name:?}: `provider` is for codex-app-server harnesses");
                }
            }
            HarnessType::CodexAppServer => {
                if !config.config.is_empty() {
                    bail!(
                        "harness {name:?}: codex-app-server takes no `config`; \
                         set Codex options in Codex's own configuration"
                    );
                }
                if config.command.is_empty() {
                    config.command = vec!["codex".into(), "app-server".into()];
                }
            }
        }
        if config.command.is_empty() {
            bail!("harness {name:?} has an empty command");
        }
        Ok((name, config))
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

    fn parse(text: &str) -> Result<Settings> {
        Ok(toml::from_str::<Config>(text)?.headless.unwrap_or_default())
    }

    #[test]
    fn activity_is_shared_unless_a_harness_opts_out() {
        let s = parse(
            "[headless.harness.open]\ncommand = [\"a\"]\n\
             [headless.harness.private]\ncommand = [\"b\"]\nshare_activity = false\n",
        )
        .unwrap();
        assert!(s.harness(Some("open")).unwrap().1.shares_activity());
        assert!(!s.harness(Some("private")).unwrap().1.shares_activity());
    }

    #[test]
    fn parses_harness_tables_and_default() {
        let s = parse(
            r#"
human = "will"

[headless]
default_harness = "omp"
mid_turn = "queue"
min_turn_interval = "2m"
max_turn_errors = 5

[headless.harness.omp]
command = ["omp", "acp"]
model = "some/model"
config = { mode = "default" }

[headless.harness.codex]
type = "acp"
command = ["npx", "-y", "@zed-industries/codex-acp"]
env = { RUST_LOG = "warn" }
"#,
        )
        .unwrap();
        assert_eq!(s.default_harness.as_deref(), Some("omp"));
        assert_eq!(s.mid_turn, Some(MidTurn::Queue));
        assert_eq!(s.min_turn_interval().unwrap(), Duration::from_secs(120));
        assert_eq!(s.max_turn_errors.map(NonZeroU32::get), Some(5));
        let (name, omp) = s.harness(None).unwrap();
        assert_eq!(name, "omp");
        assert_eq!(omp.kind, HarnessType::Acp);
        assert_eq!(omp.command, ["omp", "acp"]);
        assert_eq!(omp.model.as_deref(), Some("some/model"));
        assert_eq!(omp.config["mode"], "default");
        let (_, codex) = s.harness(Some("codex")).unwrap();
        assert_eq!(codex.kind, HarnessType::Acp);
        assert_eq!(codex.env["RUST_LOG"], "warn");
        assert!(s.harness(Some("missing")).is_err());
    }

    #[test]
    fn single_harness_is_the_default() {
        let s = parse("[headless.harness.omp]\ncommand = [\"omp\", \"acp\"]\n").unwrap();
        assert_eq!(s.harness(None).unwrap().0, "omp");
        assert_eq!(s.min_turn_interval().unwrap(), DEFAULT_MIN_TURN_INTERVAL);
    }

    #[test]
    fn codex_harness_defaults_its_command() {
        let s = parse(
            "[headless.harness.codex]\ntype = \"codex-app-server\"\nmodel = \"m\"\nprovider = \"p\"\n",
        )
        .unwrap();
        let (_, codex) = s.harness(None).unwrap();
        assert_eq!(codex.kind, HarnessType::CodexAppServer);
        assert_eq!(codex.command, ["codex", "app-server"]);
        assert_eq!(codex.provider.as_deref(), Some("p"));
        let acp_provider = parse("[headless.harness.omp]\ncommand = [\"omp\"]\nprovider = \"p\"\n");
        assert!(acp_provider.unwrap().harness(None).is_err());
        let codex_config =
            parse("[headless.harness.c]\ntype = \"codex-app-server\"\nconfig = { a = \"b\" }\n");
        assert!(codex_config.unwrap().harness(None).is_err());
        assert!(
            parse("[headless.harness.omp]\n")
                .unwrap()
                .harness(None)
                .is_err()
        );
    }

    #[test]
    fn missing_table_is_empty() {
        let s = parse("human = \"will\"\n").unwrap();
        assert!(s.harnesses.is_empty());
        assert!(s.harness(None).is_err());
    }

    #[test]
    fn process_mcp_env_is_explicit_and_rejects_incompatible_launches() {
        let default = parse("[headless.harness.a]\ncommand = [\"x\"]\n").unwrap();
        let config = default.harness(None).unwrap().1;
        assert_eq!(config.mcp_env, McpEnv::Server);
        assert!(config.check_mcp_env(Some("1")).is_ok());
        let text = "[headless.harness.a]\ncommand = [\"x\"]\nmcp_env = \"process\"\n";
        let parsed: Config = toml::from_str(text).unwrap();
        let written = toml::to_string(&parsed).unwrap();
        let mut config = parse(&written).unwrap().harness(None).unwrap().1;
        assert_eq!(config.mcp_env, McpEnv::Process);
        assert!(config.check_mcp_env(None).is_ok());
        assert!(config.check_mcp_env(Some("1")).is_err());
        config
            .env
            .insert("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "0".into());
        assert!(config.check_mcp_env(Some("1")).is_ok());
        config
            .env
            .insert("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "true".into());
        assert!(config.check_mcp_env(Some("0")).is_err());
        config.env.clear();
        config.kind = HarnessType::CodexAppServer;
        assert!(config.check_mcp_env(None).is_err());
        assert!(parse(&text.replace("process", "bogus")).is_err());
    }

    #[test]
    fn rejects_unknown_keys_and_types() {
        assert!(parse("[headless]\nbogus = 1\n").is_err());
        assert!(parse("[headless]\nmax_turn_errors = 0\n").is_err());
        assert!(parse("[headless.harness.omp]\ncommand = [\"x\"]\nbogus = 1\n").is_err());
        assert!(parse("[headless.harness.omp]\ntype = \"bogus\"\ncommand = [\"x\"]\n").is_err());
    }

    #[test]
    fn serializes_back_to_the_same_settings() {
        let text = r#"
[headless]
mid_turn = "steer"

[headless.harness.omp]
command = ["omp", "acp"]
env = { A = "1" }
"#;
        let written = toml::to_string(&toml::from_str::<Config>(text).unwrap()).unwrap();
        assert!(
            !written.contains("type"),
            "default type is not written: {written}"
        );
        let s = parse(&written).unwrap();
        assert_eq!(s.mid_turn, Some(MidTurn::Steer));
        assert_eq!(s.harnesses["omp"].command, ["omp", "acp"]);
        assert_eq!(s.harnesses["omp"].env["A"], "1");
    }
}
