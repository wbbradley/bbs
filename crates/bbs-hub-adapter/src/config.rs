//! The adapter's configuration file (TOML). Relative paths are relative to
//! the file's directory.
//!
//! ```toml
//! namespace = "bbs"            # this installation's Hub namespace
//! store = "adapter.db"         # the adapter's own SQLite store
//! # retention_days = 0         # days to keep settled handoffs; 0 (default) keeps them
//! # allow_plaintext_network = false
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
//!
//! Transport: the adapter speaks plaintext HTTP/2 only (no TLS yet), so an
//! `https://` endpoint is refused. A plaintext endpoint must be on loopback
//! unless `allow_plaintext_network = true`, which is for a network that
//! protects transport itself, such as a tailnet: otherwise the integration
//! credential and federation traffic cross the network in the clear. The
//! relay is also the trust source, by Hub's design: it pages the namespace
//! bindings this adapter trusts, so the path to it must be protected.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::verify::{valid_id, valid_namespace};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub namespace: String,
    pub store: PathBuf,
    /// How long settled handoffs are kept, in days; 0, the default, keeps them.
    #[serde(default)]
    pub retention_days: u64,
    /// Allow plaintext endpoints on hosts other than loopback.
    #[serde(default)]
    pub allow_plaintext_network: bool,
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

/// Whether `host` (from a URL) is a loopback address or `localhost`.
fn loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
            let uri: tonic::codegen::http::Uri = url
                .parse()
                .with_context(|| format!("{what} {url:?} is not a URL"))?;
            match uri.scheme_str() {
                Some("http") => {}
                Some("https") => bail!(
                    "{what} {url:?}: TLS is not supported yet. Use an http:// endpoint on \
                     loopback, or on a network that protects transport itself (such as a \
                     tailnet) with allow_plaintext_network = true"
                ),
                _ => bail!("{what} {url:?} must be an http:// URL"),
            }
            let host = uri.host().unwrap_or_default();
            if !loopback(host) && !self.allow_plaintext_network {
                bail!(
                    "{what} {url:?} is plaintext to {host}, not loopback: the integration \
                     credential and federation traffic would cross the network unencrypted. \
                     Keep it on loopback, or, on a network that protects transport itself \
                     (such as a tailnet), set allow_plaintext_network = true"
                );
            }
        }
        Ok(())
    }

    /// Settled handoffs' retention, as the adapter's options take it.
    pub fn retention(&self) -> Option<std::time::Duration> {
        (self.retention_days > 0)
            .then(|| std::time::Duration::from_secs(self.retention_days * 24 * 3600))
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

        assert_eq!(cfg.retention(), None);

        std::fs::write(&path, "namespace = \"bbs\"\nextra = 1\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    fn with(bbs: &str, relay: &str, plaintext: bool) -> Config {
        Config {
            namespace: "bbs".into(),
            store: "adapter.db".into(),
            retention_days: 0,
            allow_plaintext_network: plaintext,
            bbs: Bbs {
                endpoint: bbs.into(),
                integration_id: "hub".into(),
                credential_file: "t".into(),
            },
            hub: Hub {
                relay: relay.into(),
                installation_id: "bbs-1".into(),
                key_file: "k".into(),
                credential_file: "c".into(),
            },
        }
    }

    /// TLS is refused until it is supported; plaintext stays on loopback
    /// unless the network is declared to protect it.
    #[test]
    fn endpoints_stay_plaintext_on_loopback_unless_allowed() {
        let local = "http://127.0.0.1:7801";
        for ok in ["http://127.0.0.1:1", "http://localhost:1", "http://[::1]:1"] {
            with(ok, local, false).check().unwrap();
            with(local, ok, false).check().unwrap();
        }
        let err = with(local, "https://relay.example:443", true)
            .check()
            .unwrap_err()
            .to_string();
        assert!(err.contains("TLS is not supported"), "{err}");
        for remote in ["http://relay.example:7443", "http://100.64.0.1:7443"] {
            let err = with(local, remote, false).check().unwrap_err().to_string();
            assert!(err.contains("allow_plaintext_network"), "{err}");
            let err = with(remote, local, false).check().unwrap_err().to_string();
            assert!(err.contains("integration"), "{err}");
            with(remote, remote, true).check().unwrap();
        }
        assert!(with("ftp://x", local, true).check().is_err());
        assert_eq!(with(local, local, false).retention(), None);
    }
}
