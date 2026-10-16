//! Client configuration in `~/.config/bbs/config.toml`. A missing file means
//! local mode with no configured human.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::home_dir;
use crate::headless::Settings;

/// Version of the fields `Config` accepts. Increase it when a change means an
/// older binary can no longer read a config this one writes.
pub const FORMAT: u32 = 2;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host:port` of a `bbs serve` process. Unset means local mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// This machine's human name, used when `--as` is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human: Option<String>,
    /// Credential for `server`; never reused with a different server override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_token: Option<String>,
    /// `[headless]`: harnesses and run defaults for `bbs headless run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless: Option<Settings>,
}

pub fn path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/bbs/config.toml"))
}

/// Parse a config, reporting errors without the source snippet `toml` would
/// quote, since that can contain a credential. The message names a value only
/// when it has the wrong type, and the credential fields take any string.
fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|err| {
        let line = err
            .span()
            .map(|span| text[..span.start].matches('\n').count() + 1);
        match line {
            Some(line) => format!("line {line}: {}", err.message()),
            None => err.message().to_string(),
        }
    })
}

impl Config {
    /// The config file as written, without environment overrides.
    pub fn read() -> Result<Config> {
        let path = path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        parse(&text).map_err(|err| anyhow::anyhow!("invalid config in {}: {err}", path.display()))
    }

    /// The effective config: the file, with `BBS_SERVER` overriding `server`.
    pub fn load() -> Result<Config> {
        let mut config = Config::read()?;
        if let Some(server) = std::env::var("BBS_SERVER").ok().filter(|s| !s.is_empty()) {
            if config.server.as_ref() != Some(&server) {
                config.human_token = None;
            }
            config.server = Some(server);
        }
        Ok(config)
    }

    pub fn write(&self) -> Result<()> {
        let path = path()?;
        let dir = path.parent().expect("config path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(toml::to_string(self)?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The human name: `--as`, then the config, then `$USER`.
    pub fn human(&self, as_human: Option<&str>) -> Option<String> {
        as_human
            .map(str::to_string)
            .or_else(|| self.human.clone())
            .or_else(|| std::env::var("USER").ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors_name_the_line_but_not_the_credential() {
        let err = parse("human_token = \"secret\"\n[headless]\nmid_turn = \"sometimes\"\n")
            .err()
            .unwrap();
        assert_eq!(
            err,
            "line 3: unknown variant `sometimes`, expected `steer` or `queue`"
        );
        let err = parse("human = \"ada\"\nbogus = 1\n").err().unwrap();
        assert!(err.starts_with("line 2: unknown field `bogus`"), "{err}");
        let err = parse("human_token = 'secret\n").err().unwrap();
        assert!(!err.contains("secret"), "{err}");
    }
}
