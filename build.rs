use std::path::PathBuf;
use std::process::Command;

#[path = "build/identity.rs"]
mod identity;

/// Sources consumed by the frontend build, excluding generated dependencies
/// and bundles. Use the same paths for rebuilding and provenance.
const WEB_INPUTS: &[&str] = &[
    "web/src",
    "web/public",
    "web/index.html",
    "web/package.json",
    "web/package-lock.json",
    "web/vite.config.ts",
    "web/tsconfig.json",
    "assets/connect/logo.txt",
];

/// Tracked sources of the binary: they trigger rebuilds and define "dirty".
const INPUTS: &[&str] = &[
    "proto",
    "src",
    "build",
    "migrations",
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
    "assets",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the proto in pure Rust, so the build needs no `protoc`.
    let fds = protox::compile(["proto/bbs.proto"], ["proto"])?;
    let web = std::env::var_os("CARGO_FEATURE_WEB").is_some();
    let mut proto = tonic_prost_build::configure();
    if web {
        // The web UI's JSON API carries these messages with their proto field names.
        proto = proto.message_attribute(
            ".",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        );
    }
    proto.compile_fds(fds)?;
    if web {
        embed_web_assets()?;
    }
    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in WEB_INPUTS {
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
    let checkout = git_revision();
    let packaged = checkout.is_none().then(packaged_revision).flatten();
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
                args.extend(WEB_INPUTS);
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

/// Embed the web UI (the `web` feature) as `(path, bytes)` pairs. From a
/// source checkout, build it with npm into `OUT_DIR`; the build fails without
/// npm. `BBS_WEB_DIST` names a bundle built elsewhere instead (the container
/// build), and a published package, which ships no web sources, embeds the
/// `web/dist` it was packaged with.
fn embed_web_assets() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("no manifest dir")?);
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("no OUT_DIR")?);
    let web = manifest.join("web");
    println!("cargo:rerun-if-env-changed=BBS_WEB_DIST");
    let dist = if let Some(dist) = std::env::var_os("BBS_WEB_DIST") {
        let dist = PathBuf::from(dist);
        println!("cargo:rerun-if-changed={}", dist.display());
        dist
    } else if web.join("package.json").is_file() {
        // Rebuild whenever Cargo reruns this script, including when an input
        // was deleted. The output directory is emptied by the bundler.
        let dist = out_dir.join("web-dist");
        npm_build(&web, &dist)?;
        dist
    } else {
        println!("cargo:rerun-if-changed=web/dist");
        web.join("dist")
    };
    if !dist.join("index.html").is_file() {
        return Err(format!("no web UI build at {}", dist.display()).into());
    }
    let mut files = Vec::new();
    collect(&dist, &dist, &mut files)?;
    files.sort();
    let mut out = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (rel, path) in files {
        out.push_str(&format!("    ({rel:?}, include_bytes!({path:?})),\n"));
    }
    out.push_str("];\n");
    std::fs::write(out_dir.join("web_assets.rs"), out)?;
    Ok(())
}

/// Install exactly the locked dependencies, then type-check and bundle.
fn npm_build(
    web: &std::path::Path,
    out: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    npm(web, &["ci", "--no-audit", "--no-fund"])?;
    let out = out.to_str().ok_or("OUT_DIR is not UTF-8")?;
    npm(
        web,
        &["run", "build", "--", "--outDir", out, "--emptyOutDir"],
    )
}

fn npm(web: &std::path::Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("npm")
        .args(args)
        .current_dir(web)
        // Cargo reads build-script stdout as directives; keep npm's on stderr.
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .status()
        .map_err(|e| format!("the `web` feature needs npm (Node.js 20.19 or newer): {e}"))?;
    if !status.success() {
        return Err(format!("`npm {}` failed in {}", args.join(" "), web.display()).into());
    }
    Ok(())
}

fn collect(
    root: &std::path::Path,
    dir: &std::path::Path,
    files: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            files.push((rel, path.to_string_lossy().into_owned()));
        }
    }
    Ok(())
}

/// HEAD of this package's own checkout, never of an enclosing repository.
fn git_revision() -> Option<String> {
    let toplevel = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    if toplevel.canonicalize().ok()? != manifest_dir.canonicalize().ok()? {
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
