//! `bbs setup` against isolated harness directories, without touching user config.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const REPO: &str = env!("CARGO_MANIFEST_DIR");

struct Setup {
    _temp: tempfile::TempDir,
    root: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    skills: PathBuf,
    settings: PathBuf,
    hooks: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("bbs-setup-")
            .tempdir()
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (claude, codex) = (root.join("claude"), root.join("codex"));
        Setup {
            settings: claude.join("settings.json"),
            hooks: codex.join("hooks.json"),
            skills: root.join("skills"),
            claude,
            codex,
            root,
            _temp: temp,
        }
    }

    /// Run with explicit directories, or with only the environment defaults.
    fn run(&self, extra: &[&str], use_env: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bbs"));
        cmd.arg("setup")
            .args(extra)
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex);
        if !use_env {
            cmd.arg("--claude-dir")
                .arg(&self.claude)
                .arg("--codex-dir")
                .arg(&self.codex)
                .arg("--skills-dir")
                .arg(&self.skills);
        }
        cmd.output().unwrap()
    }

    fn ok(&self, extra: &[&str]) -> String {
        let out = self.run(extra, false);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn err(&self) -> String {
        let out = self.run(&[], false);
        assert!(!out.status.success());
        String::from_utf8(out.stderr).unwrap()
    }
}

fn template(harness: &str) -> Value {
    read_json(
        &Path::new(REPO)
            .join("integrations")
            .join(harness)
            .join("hooks.json"),
    )
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write_json(path: &Path, data: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{data}\n")).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The skill at `dir` is a real directory holding the repo's skill files.
fn assert_skill(dir: &Path) {
    assert!(!dir.is_symlink(), "{}", dir.display());
    for file in ["SKILL.md", "agents/openai.yaml"] {
        let source = Path::new(REPO).join("integrations/skills/bbs").join(file);
        assert_eq!(fs::read(dir.join(file)).unwrap(), fs::read(source).unwrap());
    }
}

fn snapshot(paths: &[PathBuf]) -> Vec<(Vec<u8>, std::time::SystemTime)> {
    paths
        .iter()
        .map(|p| {
            (
                fs::read(p).unwrap(),
                fs::metadata(p).unwrap().modified().unwrap(),
            )
        })
        .collect()
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(backups(&path));
        } else if path.to_string_lossy().ends_with(".bak-bbs") {
            found.push(path);
        }
    }
    found
}

#[test]
fn fresh_install_and_repeat_leave_files_untouched() {
    let s = Setup::new();
    let out = s.ok(&[]);
    assert!(
        out.contains(&format!("Update {}", s.settings.display())),
        "{out}"
    );
    assert!(out.contains("Rerun `bbs setup` after upgrading"), "{out}");
    for (path, harness) in [(&s.settings, "claude-code"), (&s.hooks, "codex")] {
        assert_eq!(read_json(path), template(harness));
        assert_eq!(mode(path), 0o600);
    }
    assert_skill(&s.skills.join("bbs"));
    assert_skill(&s.claude.join("skills/bbs"));
    let paths = [
        s.settings.clone(),
        s.hooks.clone(),
        s.skills.join("bbs/SKILL.md"),
        s.claude.join("skills/bbs/agents/openai.yaml"),
    ];
    let before = snapshot(&paths);
    let out = s.ok(&[]);
    assert!(!out.contains("Update") && !out.contains("Install"), "{out}");
    assert_eq!(snapshot(&paths), before);
    assert!(backups(&s.root).is_empty());
}

#[test]
fn preserves_settings_custom_handlers_order_permissions_and_backup() {
    let s = Setup::new();
    let custom = json!({"type": "command", "command": "bbs hook claude-code", "timeout": 30});
    let group = json!({"matcher": "", "hooks": [{"type": "command", "command": "notify"}, custom]});
    let original = json!({
        "model": "custom-model",
        "permissions": {"allow": ["Read(*)"]},
        "hooks": {"Stop": [group], "PreToolUse": [{"hooks": []}]},
    });
    // Keys out of alphabetical order, to check that order survives.
    fs::create_dir_all(&s.claude).unwrap();
    let text = r#"{"zeta": 1, "alpha": 2, "hooks": {"Stop": [], "PreToolUse": []}}"#;
    fs::write(&s.settings, text).unwrap();
    s.ok(&[]);
    let keys: Vec<String> = read_json(&s.settings)
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["zeta", "alpha", "hooks"]);
    fs::remove_file(s.claude.join("settings.json.bak-bbs")).unwrap();

    write_json(&s.settings, &original);
    fs::set_permissions(&s.settings, fs::Permissions::from_mode(0o640)).unwrap();
    let original_bytes = fs::read(&s.settings).unwrap();
    s.ok(&[]);
    let mut installed = read_json(&s.settings);
    assert_eq!(installed["model"], original["model"]);
    assert_eq!(installed["permissions"], original["permissions"]);
    assert_eq!(installed["hooks"]["Stop"], json!([group]));
    assert_eq!(
        installed["hooks"]["PreToolUse"],
        original["hooks"]["PreToolUse"]
    );
    assert_eq!(mode(&s.settings), 0o640);
    let backup = s.claude.join("settings.json.bak-bbs");
    assert_eq!(fs::read(&backup).unwrap(), original_bytes);
    // Later rewrites keep the first backup.
    installed["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("SessionStart");
    write_json(&s.settings, &installed);
    s.ok(&[]);
    assert_eq!(fs::read(&backup).unwrap(), original_bytes);
}

#[test]
fn removes_unmodified_handlers_from_retired_events() {
    let s = Setup::new();
    let old = json!({"type": "command", "command": "bbs hook codex", "timeout": 10});
    let custom = json!({"type": "command", "command": "bbs hook codex", "timeout": 30});
    let other = json!({"type": "command", "command": "notify"});
    write_json(
        &s.hooks,
        &json!({"hooks": {
            "UserPromptSubmit": [{"hooks": [old]}],
            "Stop": [{"hooks": [other, old]}, {"hooks": [custom]}, {"hooks": [old]}],
            "PreToolUse": [{"hooks": []}],
        }}),
    );
    s.ok(&[]);
    let hooks = &read_json(&s.hooks)["hooks"];
    assert!(hooks.get("UserPromptSubmit").is_none());
    assert_eq!(
        hooks["Stop"],
        json!([{"hooks": [other]}, {"hooks": [custom]}])
    );
    assert_eq!(hooks["PreToolUse"], json!([{"hooks": []}]));
}

#[test]
fn config_symlinks_and_shared_skill_directory() {
    let s = Setup::new();
    let target = s.root.join("dotfiles/settings.json");
    write_json(&target, &json!({"model": "keep"}));
    fs::create_dir_all(&s.claude).unwrap();
    symlink(&target, &s.settings).unwrap();
    // An older install linked the skill to a checkout; the checkout is left alone.
    let checkout = s.root.join("old-checkout");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("SKILL.md"), "old").unwrap();
    fs::create_dir_all(&s.skills).unwrap();
    symlink(&s.skills, s.claude.join("skills")).unwrap();
    symlink(&checkout, s.skills.join("bbs")).unwrap();
    let out = s.ok(&[]);
    assert_eq!(out.matches("Install skill").count(), 1, "{out}");
    assert!(s.settings.is_symlink());
    assert_eq!(read_json(&target)["model"], "keep");
    assert!(s.root.join("dotfiles/settings.json.bak-bbs").exists());
    assert_skill(&s.skills.join("bbs"));
    assert_eq!(
        fs::read_to_string(checkout.join("SKILL.md")).unwrap(),
        "old"
    );
}

#[test]
fn existing_skill_directory_keeps_other_files() {
    let s = Setup::new();
    let existing = s.skills.join("bbs");
    fs::create_dir_all(&existing).unwrap();
    fs::write(existing.join("SKILL.md"), "local skill").unwrap();
    fs::write(existing.join("notes.md"), "mine").unwrap();
    s.ok(&[]);
    assert_skill(&existing);
    assert_eq!(
        fs::read_to_string(existing.join("notes.md")).unwrap(),
        "mine"
    );
}

#[test]
fn dry_run_and_environment_defaults() {
    let s = Setup::new();
    let out = s.run(&["--dry-run"], true);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_dir(&s.root).unwrap().count(), 0);
    assert!(s.run(&[], true).status.success());
    assert!(s.settings.exists() && s.hooks.exists());
    assert_skill(&s.root.join(".agents/skills/bbs"));
    assert_skill(&s.claude.join("skills/bbs"));
}

#[test]
fn invalid_config_is_rejected_before_any_writes() {
    for invalid in [
        "{",
        "null",
        r#"{"hooks": []}"#,
        r#"{"hooks": {"Stop": [{}]}}"#,
    ] {
        let s = Setup::new();
        fs::create_dir_all(&s.codex).unwrap();
        fs::write(&s.hooks, invalid).unwrap();
        let err = s.err();
        assert!(err.contains(&s.hooks.display().to_string()), "{err}");
        assert_eq!(fs::read_to_string(&s.hooks).unwrap(), invalid);
        assert!(!s.settings.exists());
        assert!(!s.skills.exists());
    }
}

#[test]
fn dangling_config_symlink_and_skill_file_are_rejected_before_any_writes() {
    let s = Setup::new();
    fs::create_dir_all(&s.claude).unwrap();
    symlink(s.root.join("missing.json"), &s.settings).unwrap();
    assert!(s.err().contains("dangling configuration symlink"));
    assert!(!s.hooks.exists());
    fs::remove_file(&s.settings).unwrap();

    fs::create_dir_all(&s.skills).unwrap();
    fs::write(s.skills.join("bbs"), "not a directory").unwrap();
    assert!(s.err().contains("not a directory; move it aside"));
    assert!(!s.settings.exists() && !s.hooks.exists());
}
