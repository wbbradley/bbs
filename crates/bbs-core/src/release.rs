//! The running executable's identity, embedded by its own `build.rs` and
//! handed in with [`init`] before anything reports it. `ServerInfo` and
//! `version` both report from here.

use std::sync::OnceLock;

use serde::Serialize;

use crate::{db, proto, protocol};

/// The workspace's version: every bbs crate is released together at it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Version of the `bbs-release.json` layout; bump on incompatible changes.
const MANIFEST_FORMAT: u32 = 1;
/// Version of the fields the client's `~/.config/bbs/config.toml` accepts.
/// Increase it when a change means an older binary can no longer read a
/// config this one writes.
pub const CONFIG_FORMAT: u32 = 4;

/// What an executable was built from, as its `build.rs` found it.
#[derive(Clone, Copy, Debug)]
pub struct Identity {
    /// The executable: `bbs` or `bbs-server`.
    pub name: &'static str,
    pub version: &'static str,
    pub revision: &'static str,
    pub target: &'static str,
    /// `true`, `false`, or empty when unknown.
    pub dirty: &'static str,
    pub rust_version: &'static str,
}

/// The calling crate's identity from its build script's environment; an
/// executable passes it to [`init`] first thing.
#[macro_export]
macro_rules! identity {
    () => {
        $crate::release::Identity {
            name: env!("CARGO_BIN_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            revision: env!("BBS_BUILD_REVISION"),
            target: env!("BBS_BUILD_TARGET"),
            dirty: env!("BBS_BUILD_DIRTY"),
            rust_version: env!("CARGO_PKG_RUST_VERSION"),
        }
    };
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// Record the executable's identity; the first call wins. Its version must
/// be the workspace's, so no crate reports a release it is not part of.
pub fn init(identity: Identity) {
    assert_eq!(
        identity.version, VERSION,
        "every bbs crate is released at one version"
    );
    let _ = IDENTITY.set(identity);
}

/// The executable's identity; without [`init`], as in a library's own
/// tests, an unknown revision, which no release can be made from.
fn identity() -> &'static Identity {
    IDENTITY.get_or_init(|| Identity {
        name: "bbs-core",
        version: VERSION,
        revision: "unknown",
        target: "unknown",
        dirty: "",
        rust_version: env!("CARGO_PKG_RUST_VERSION"),
    })
}

pub fn revision() -> &'static str {
    identity().revision
}

pub fn target() -> &'static str {
    identity().target
}

/// Whether tracked build inputs differed from the revision; unknown with the revision.
pub fn dirty() -> Option<bool> {
    parse_dirty(identity().dirty)
}

fn parse_dirty(value: &str) -> Option<bool> {
    value.parse().ok()
}

/// Clean builds of a known revision are the only candidates for official
/// releases; dirty and unknown builds are development builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Clean,
    Dirty,
    Unknown,
}

impl Provenance {
    fn of(revision: &str, dirty: Option<bool>) -> Provenance {
        match (revision, dirty) {
            ("unknown", _) | (_, None) => Provenance::Unknown,
            (_, Some(true)) => Provenance::Dirty,
            (_, Some(false)) => Provenance::Clean,
        }
    }
}

pub fn info() -> proto::ReleaseInfo {
    proto::ReleaseInfo {
        version: VERSION.into(),
        revision: revision().into(),
        dirty: dirty(),
    }
}

#[cfg(test)]
#[path = "../build/identity.rs"]
mod build_identity;

#[derive(Serialize)]
struct Protocol {
    epoch: u32,
    capabilities: &'static [&'static str],
}

#[derive(Serialize)]
struct Report {
    version: &'static str,
    revision: &'static str,
    dirty: Option<bool>,
    provenance: Provenance,
    target: &'static str,
    protocol: Protocol,
    schema_version: usize,
    config_format: u32,
}

/// The `bbs-release.json` asset of a published release: what a trusted catalog
/// entry promises about the release's source and compatibility.
#[derive(Serialize)]
struct Manifest {
    format: u32,
    version: &'static str,
    tag: String,
    revision: &'static str,
    rust_version: &'static str,
    protocol: Protocol,
    schema_version: usize,
    config_format: u32,
}

/// Only clean builds of a known revision describe a publishable release.
fn manifest(revision: &'static str, dirty: Option<bool>) -> anyhow::Result<Manifest> {
    let provenance = Provenance::of(revision, dirty);
    anyhow::ensure!(
        provenance == Provenance::Clean,
        "a release manifest needs a clean build of a known revision; this build is {}",
        format!("{provenance:?}").to_lowercase(),
    );
    Ok(Manifest {
        format: MANIFEST_FORMAT,
        version: VERSION,
        tag: format!("v{VERSION}"),
        revision,
        rust_version: identity().rust_version,
        protocol: protocol(),
        schema_version: db::SCHEMA_VERSION,
        config_format: CONFIG_FORMAT,
    })
}

fn protocol() -> Protocol {
    Protocol {
        epoch: protocol::EPOCH,
        capabilities: protocol::CAPABILITIES,
    }
}

fn report() -> Report {
    Report {
        version: VERSION,
        revision: revision(),
        dirty: dirty(),
        provenance: Provenance::of(revision(), dirty()),
        target: target(),
        protocol: protocol(),
        schema_version: db::SCHEMA_VERSION,
        config_format: CONFIG_FORMAT,
    }
}

/// `bbs version`: reads only compiled-in constants, so it opens no files,
/// configuration, database or connection.
pub fn print(json: bool, manifest: bool) -> anyhow::Result<()> {
    if manifest {
        println!(
            "{}",
            serde_json::to_string(&self::manifest(revision(), dirty())?)?
        );
        return Ok(());
    }
    let report = report();
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        let source = match (report.provenance, report.revision) {
            (_, "unknown") => "unknown revision".to_string(),
            (Provenance::Clean, revision) => revision.to_string(),
            (Provenance::Dirty, revision) => format!("{revision} with local changes"),
            (Provenance::Unknown, revision) => format!("{revision}, changes unknown"),
        };
        println!(
            "{} {} ({source}, {})\nprotocol bbs.v{}: {}\nschema {}, config format {}",
            identity().name,
            report.version,
            report.target,
            report.protocol.epoch,
            report.protocol.capabilities.join(", "),
            report.schema_version,
            report.config_format,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_requires_known_revision_and_dirty_state() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(Provenance::of(rev, Some(false)), Provenance::Clean);
        assert_eq!(Provenance::of(rev, Some(true)), Provenance::Dirty);
        assert_eq!(Provenance::of(rev, None), Provenance::Unknown);
        assert_eq!(Provenance::of("unknown", Some(false)), Provenance::Unknown);
    }

    #[test]
    fn embedded_dirty_flag_is_empty_when_unknown() {
        assert_eq!(parse_dirty("true"), Some(true));
        assert_eq!(parse_dirty("false"), Some(false));
        assert_eq!(parse_dirty(""), None);
        // A library's own tests run without an executable's identity.
        assert_eq!(revision(), "unknown");
        assert_eq!(dirty(), None);
    }

    #[test]
    fn report_uses_shared_constants() {
        let value = serde_json::to_value(report()).unwrap();
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["revision"], revision());
        assert_eq!(value["target"], target());
        assert_eq!(value["protocol"]["epoch"], protocol::EPOCH);
        assert_eq!(
            value["protocol"]["capabilities"],
            serde_json::json!(protocol::CAPABILITIES)
        );
        assert_eq!(value["schema_version"], db::SCHEMA_VERSION);
        assert_eq!(value["config_format"], CONFIG_FORMAT);
        assert!(revision() == "unknown" || revision().len() == 40);
    }

    #[test]
    fn manifest_requires_clean_known_revision() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let value = serde_json::to_value(manifest(rev, Some(false)).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "format": 1,
                "version": VERSION,
                "tag": format!("v{VERSION}"),
                "revision": rev,
                "rust_version": env!("CARGO_PKG_RUST_VERSION"),
                "protocol": {"epoch": protocol::EPOCH, "capabilities": protocol::CAPABILITIES},
                "schema_version": db::SCHEMA_VERSION,
                "config_format": CONFIG_FORMAT,
            })
        );
        for (revision, dirty) in [(rev, Some(true)), (rev, None), ("unknown", Some(false))] {
            assert!(manifest(revision, dirty).is_err());
        }
    }

    #[test]
    fn build_identity_overrides_and_unknown_revisions() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let git_rev = || Some(rev.to_string());
        let unreachable_rev = || -> Option<String> { panic!("git consulted") };
        let unreachable_dirty = || -> Option<bool> { panic!("git consulted") };
        // Overrides win without consulting git.
        assert_eq!(
            build_identity::resolve(Some(rev), Some("false"), unreachable_rev, unreachable_dirty),
            Ok((rev.into(), Some(false)))
        );
        // Unknown revisions never carry a dirty flag.
        assert_eq!(
            build_identity::resolve(
                Some("unknown"),
                Some("true"),
                unreachable_rev,
                unreachable_dirty
            ),
            Ok(("unknown".into(), None))
        );
        assert_eq!(
            build_identity::resolve(None, None, || None, unreachable_dirty),
            Ok(("unknown".into(), None))
        );
        assert_eq!(
            build_identity::resolve(None, None, || Some("abc123".into()), unreachable_dirty),
            Ok(("unknown".into(), None))
        );
        // Git supplies both when there is no override.
        assert_eq!(
            build_identity::resolve(None, None, git_rev, || Some(true)),
            Ok((rev.into(), Some(true)))
        );
        // An overridden revision ignores git's working tree.
        assert_eq!(
            build_identity::resolve(Some(rev), None, unreachable_rev, unreachable_dirty),
            Ok((rev.into(), None))
        );
        assert_eq!(
            build_identity::resolve(None, None, git_rev, || None),
            Ok((rev.into(), None))
        );
        for (revision, dirty) in [
            (Some("abc123"), None),
            (Some(&rev.to_uppercase()[..]), None),
            (Some(rev), Some("yes")),
            (None, Some("")),
        ] {
            assert!(build_identity::resolve(revision, dirty, git_rev, || Some(false)).is_err());
        }
    }

    #[test]
    fn server_info_reports_this_build() {
        let info = protocol::Policy::default().info(false);
        assert_eq!(info.release, Some(super::info()));
    }
}
