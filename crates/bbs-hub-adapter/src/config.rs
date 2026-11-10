//! The adapter's configuration file (TOML). Relative paths are relative to
//! the file's directory.
//!
//! ```toml
//! namespace = "bbs"            # this installation's Hub namespace
//! store = "adapter.db"         # the adapter's own SQLite store
//!
//! [bbs]
//! endpoint = "http://127.0.0.1:7801"   # bbs serve --integration-listen
//! integration_id = "hub"               # bbs admin integration add hub
//! credential_file = "integration.token"
//!
//! [hub]
//! relay = "http://127.0.0.1:7443"      # the Hub relay (and trust source)
//! installation_id = "bbs-1"
//! key_file = "installation.key"        # bbs-hub-adapter keygen --out ...
//! credential_file = "installation.cred"
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::verify::{valid_id, valid_namespace};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub namespace: String,
    pub store: PathBuf,
    pub bbs: Bbs,
    pub hub: Hub,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bbs {
    pub endpoint: String,
    pub integration_id: String,
    pub credential_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hub {
    pub relay: String,
    pub installation_id: String,
    pub key_file: PathBuf,
    pub credential_file: PathBuf,
}

impl Config {
    /// Read and check a configuration file.
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        for p in [
            &mut cfg.store,
            &mut cfg.bbs.credential_file,
            &mut cfg.hub.key_file,
            &mut cfg.hub.credential_file,
        ] {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        cfg.check()?;
        Ok(cfg)
    }

    pub fn check(&self) -> Result<()> {
        if !valid_namespace(&self.namespace) {
            bail!(
                "namespace {:?} is not a Hub namespace ([a-z][a-z0-9-], at most 63)",
                self.namespace
            );
        }
        if !valid_id(&self.bbs.integration_id) {
            bail!(
                "bbs.integration_id {:?} is not a valid ID",
                self.bbs.integration_id
            );
        }
        if !valid_id(&self.hub.installation_id) {
            bail!(
                "hub.installation_id {:?} is not a valid ID",
                self.hub.installation_id
            );
        }
        for (what, url) in [
            ("bbs.endpoint", &self.bbs.endpoint),
            ("hub.relay", &self.hub.relay),
        ] {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("{what} {url:?} must be an http:// or https:// URL");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_with_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("adapter.toml");
        std::fs::write(
            &path,
            r#"
namespace = "bbs"
store = "adapter.db"
[bbs]
endpoint = "http://127.0.0.1:7801"
integration_id = "hub"
credential_file = "/etc/integration.token"
[hub]
relay = "http://127.0.0.1:7443"
installation_id = "bbs-1"
key_file = "installation.key"
credential_file = "installation.cred"
"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.store, dir.path().join("adapter.db"));
        assert_eq!(
            cfg.bbs.credential_file,
            PathBuf::from("/etc/integration.token")
        );
        assert_eq!(cfg.hub.key_file, dir.path().join("installation.key"));

        std::fs::write(&path, "namespace = \"bbs\"\nextra = 1\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
