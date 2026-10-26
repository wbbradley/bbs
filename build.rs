use std::path::PathBuf;
use std::process::Command;

#[path = "build/identity.rs"]
mod identity;

/// Tracked sources of `bbs`, the shared core's included: they trigger
/// rebuilds and define "dirty".
const INPUTS: &[&str] = &[
    "src",
    "build",
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
    "assets",
    "integrations",
    "crates/bbs-core",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }

    // Builders without `.git`, such as the container build, pass the revision
    // and dirty flag explicitly.
    println!("cargo:rerun-if-env-changed=BBS_BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=BBS_BUILD_DIRTY");
    let revision_override = std::env::var("BBS_BUILD_REVISION").ok();
    let dirty_override = std::env::var("BBS_BUILD_DIRTY").ok();
    // A published package has no `.git`; Cargo records its commit in
    // `.cargo_vcs_info.json` instead.
    // A packaged source says what it was packaged from; only without that
    // is the enclosing checkout asked.
    let packaged = packaged_revision();
    let checkout = packaged.is_none().then(git_revision).flatten();
    let (revision, dirty) = identity::resolve(
        revision_override.as_deref(),
        dirty_override.as_deref(),
        || {
            checkout
                .clone()
                .or(packaged.as_ref().map(|(r, _)| r.clone()))
        },
        || match &packaged {
            Some((_, dirty)) => Some(*dirty),
            None => {
                let mut args = vec!["status", "--porcelain", "--untracked-files=normal", "--"];
                args.extend(INPUTS);
                git(&args).map(|s| !s.is_empty())
            }
        },
    )?;
    println!("cargo:rustc-env=BBS_BUILD_REVISION={revision}");
    // Always set, empty when unknown, so a builder's own BBS_BUILD_DIRTY can
    // never reach the compiled-in value.
    let dirty = dirty.map(|d| d.to_string()).unwrap_or_default();
    println!("cargo:rustc-env=BBS_BUILD_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=BBS_BUILD_TARGET={}",
        std::env::var("TARGET")?
    );
    Ok(())
}

/// HEAD of the bbs workspace this package roots, never of an enclosing
/// repository that merely contains it.
fn git_revision() -> Option<String> {
    let toplevel = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .ok()?;
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?)
        .canonicalize()
        .ok()?;
    if toplevel != manifest_dir || !toplevel.join("crates/bbs-core/Cargo.toml").is_file() {
        return None;
    }
    // Rebuild when the checked-out commit or the index moves; the HEAD reflog
    // and loose branch ref record commits, checkouts and resets. `--git-path` resolves these inside
    // linked worktrees. Cargo treats a missing path as always changed, so only
    // existing files are watched.
    let mut paths = vec!["HEAD".to_string(), "logs/HEAD".into(), "index".into()];
    paths.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for path in paths {
        if let Some(resolved) = git(&["rev-parse", "--git-path", &path])
            && std::path::Path::new(&resolved).exists()
        {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
    git(&["rev-parse", "--verify", "HEAD"])
}

/// The commit and dirty flag `cargo package` recorded, if this is a package.
fn packaged_revision() -> Option<(String, bool)> {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    // Watch it only when present: Cargo treats a missing path as always changed.
    let text = std::fs::read_to_string(dir.join(".cargo_vcs_info.json")).ok()?;
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    let info: serde_json::Value = serde_json::from_str(&text).ok()?;
    let git = info.get("git")?;
    let revision = git.get("sha1")?.as_str()?.to_string();
    let dirty = git.get("dirty").and_then(|d| d.as_bool()).unwrap_or(false);
    Some((revision, dirty))
}

fn git(args: &[&str]) -> Option<String> {
    // Optional locks would let `git status` rewrite the watched index.
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| stdout.trim().to_string())
}
