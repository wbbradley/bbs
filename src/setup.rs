//! `bbs setup`: install the BBS hooks and skill for Claude Code and Codex.
//!
//! The hook templates and skill files are compiled in, so an installed binary
//! needs no checkout. Existing settings keep their keys, order, custom handlers,
//! and file mode; everything is validated before anything is written.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::app::home_dir;
use crate::cli::SetupArgs;

/// Hook templates for each harness.
const CLAUDE_HOOKS: &str = include_str!("../integrations/claude-code/hooks.json");
const CODEX_HOOKS: &str = include_str!("../integrations/codex/hooks.json");

/// The skill's files, relative to its directory.
const SKILL_FILES: [(&str, &str); 2] = [
    (
        "SKILL.md",
        include_str!("../integrations/skills/bbs/SKILL.md"),
    ),
    (
        "agents/openai.yaml",
        include_str!("../integrations/skills/bbs/agents/openai.yaml"),
    ),
];

pub fn run(args: &SetupArgs) -> Result<()> {
    let dir = |flag: &Option<PathBuf>, var: &str, default: &str| -> Result<PathBuf> {
        Ok(match (flag, std::env::var_os(var)) {
            (Some(dir), _) => dir.clone(),
            (None, Some(dir)) => PathBuf::from(dir),
            (None, None) => home_dir()?.join(default),
        })
    };
    let claude = dir(&args.claude_dir, "CLAUDE_CONFIG_DIR", ".claude")?;
    let codex = dir(&args.codex_dir, "CODEX_HOME", ".codex")?;
    let skills = match &args.skills_dir {
        Some(dir) => dir.clone(),
        None => home_dir()?.join(".agents/skills"),
    };

    // Validate all inputs and skill destinations before changing anything.
    let mut configs = Vec::new();
    for (path, template) in [
        (claude.join("settings.json"), CLAUDE_HOOKS),
        (codex.join("hooks.json"), CODEX_HOOKS),
    ] {
        let current = load_config(&path)?;
        let merged = merge_hooks(&current, &parse(template).expect("valid hook template"));
        configs.push((path, current, merged));
    }
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    for parent in [skills, claude.join("skills")] {
        // Resolve the parent, not the skill itself; the skill directory may be shared.
        let dir = resolve(&parent).join("bbs");
        if skill_dirs.contains(&dir) {
            continue;
        }
        match fs::symlink_metadata(&dir) {
            Ok(meta) if !meta.is_symlink() && !meta.is_dir() => {
                bail!("{}: not a directory; move it aside", dir.display())
            }
            Err(err) if err.kind() != ErrorKind::NotFound => {
                return Err(err).with_context(|| dir.display().to_string());
            }
            _ => {}
        }
        skill_dirs.push(dir);
    }

    for (path, current, merged) in &configs {
        let changed = current != merged;
        println!(
            "{} {}",
            if changed { "Update" } else { "Unchanged" },
            path.display()
        );
        if changed && !args.dry_run {
            write_config(path, merged)?;
        }
    }
    for dir in &skill_dirs {
        let changed = dir.is_symlink()
            || SKILL_FILES
                .iter()
                .any(|(file, body)| !same(&dir.join(file), body));
        println!(
            "{} {}",
            if changed {
                "Install skill"
            } else {
                "Unchanged"
            },
            dir.display()
        );
        if changed && !args.dry_run {
            install_skill(dir)?;
        }
    }
    println!(
        "BBS must be on PATH for both harnesses. Review new Codex hooks with /hooks.\n\
         Rerun `bbs setup` after upgrading bbs to update the hooks and skill."
    );
    Ok(())
}

/// A missing file is an empty configuration; a dangling symlink is an error.
fn load_config(path: &Path) -> Result<Value> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if path.is_symlink() {
                bail!("{}: dangling configuration symlink", path.display());
            }
            return Ok(Value::Object(Map::new()));
        }
        Err(err) => return Err(err).with_context(|| path.display().to_string()),
    };
    parse(&text).with_context(|| path.display().to_string())
}

/// Parse a configuration and check the shape of its `hooks`.
fn parse(text: &str) -> Result<Value> {
    let config: Value = serde_json::from_str(text)?;
    let Some(object) = config.as_object() else {
        bail!("configuration must be a JSON object");
    };
    let Some(hooks) = object.get("hooks") else {
        return Ok(config);
    };
    let Some(hooks) = hooks.as_object() else {
        bail!("hooks must be an object");
    };
    for (event, groups) in hooks {
        let Some(groups) = groups.as_array() else {
            bail!("hooks.{event} must be an array");
        };
        for group in groups {
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                bail!("hooks.{event} entries need a hooks array");
            };
            if !handlers.iter().all(Value::is_object) {
                bail!("hooks.{event} handlers must be objects");
            }
        }
    }
    Ok(config)
}

fn handlers(group: &Value) -> &Vec<Value> {
    group["hooks"].as_array().expect("validated")
}

fn matcher(group: &Value) -> Option<&Value> {
    group.get("matcher").filter(|m| m.as_str() != Some(""))
}

/// Keep existing handlers and ordering, including customized BBS handlers;
/// append missing template handlers, and drop unmodified BBS handlers from
/// events the template no longer uses.
fn merge_hooks(config: &Value, template: &Value) -> Value {
    let wanted = template["hooks"].as_object().expect("validated");
    let mut merged = config.clone();
    let hooks = merged
        .as_object_mut()
        .expect("validated")
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated");
    for (event, wanted_groups) in wanted {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("validated");
        for want in wanted_groups.as_array().expect("validated") {
            let missing: Vec<Value> = handlers(want)
                .iter()
                .filter(|handler| {
                    !groups.iter().any(|group| {
                        matcher(group) == matcher(want)
                            && handlers(group).iter().any(|existing| {
                                existing.get("type") == handler.get("type")
                                    && existing.get("command") == handler.get("command")
                            })
                    })
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                let mut group = want.clone();
                group["hooks"] = Value::Array(missing);
                groups.push(group);
            }
        }
    }
    let ours: Vec<&Value> = wanted
        .values()
        .flat_map(|groups| groups.as_array().expect("validated"))
        .flat_map(handlers)
        .collect();
    hooks.retain(|event, groups| {
        if wanted.contains_key(event) {
            return true;
        }
        let groups = groups.as_array_mut().expect("validated");
        groups.retain_mut(|group| {
            let handlers = group["hooks"].as_array_mut().expect("validated");
            // Drop a group only when it held nothing but BBS handlers.
            if handlers.is_empty() {
                return true;
            }
            handlers.retain(|handler| !ours.contains(&handler));
            !handlers.is_empty()
        });
        !groups.is_empty()
    });
    merged
}

/// Write atomically, following config symlinks so dotfiles-managed settings
/// stay linked. The first rewrite keeps a `.bak-bbs` copy of the original.
fn write_config(path: &Path, config: &Value) -> Result<()> {
    let target = resolve(path);
    let parent = target
        .parent()
        .context("configuration path has no parent")?;
    let name = target
        .file_name()
        .context("configuration path has no file name")?
        .to_string_lossy();
    fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
    let mode = match fs::metadata(&target) {
        Ok(meta) => meta.permissions().mode() & 0o777,
        Err(err) if err.kind() == ErrorKind::NotFound => 0o600,
        Err(err) => return Err(err).with_context(|| target.display().to_string()),
    };
    let backup = target.with_file_name(format!("{name}.bak-bbs"));
    if target.exists() && fs::symlink_metadata(&backup).is_err() {
        fs::copy(&target, &backup).with_context(|| backup.display().to_string())?;
    }
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{name}."))
        .tempfile_in(parent)?;
    file.write_all(format!("{}\n", serde_json::to_string_pretty(config)?).as_bytes())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.persist(&target)
        .with_context(|| target.display().to_string())?;
    Ok(())
}

/// Write the skill as real files. A symlink (from an older checkout-based
/// install) is replaced; other files in an existing directory are kept.
fn install_skill(dir: &Path) -> Result<()> {
    if dir.is_symlink() {
        fs::remove_file(dir).with_context(|| dir.display().to_string())?;
    }
    for (file, body) in SKILL_FILES {
        let path = dir.join(file);
        if same(&path, body) {
            continue;
        }
        fs::create_dir_all(path.parent().expect("skill file has a parent"))?;
        fs::write(&path, body).with_context(|| path.display().to_string())?;
    }
    Ok(())
}

fn same(path: &Path, body: &str) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == body.as_bytes())
}

/// Resolve symlinks in the longest existing prefix of `path`.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn templates_are_valid_hook_configs() {
        for template in [CLAUDE_HOOKS, CODEX_HOOKS] {
            let template = parse(template).unwrap();
            assert_eq!(merge_hooks(&template, &template), template);
        }
    }

    #[test]
    fn parse_rejects_malformed_hooks() {
        for (text, error) in [
            ("null", "configuration must be a JSON object"),
            (r#"{"hooks": []}"#, "hooks must be an object"),
            (r#"{"hooks": {"Stop": {}}}"#, "hooks.Stop must be an array"),
            (
                r#"{"hooks": {"Stop": [{}]}}"#,
                "hooks.Stop entries need a hooks array",
            ),
            (
                r#"{"hooks": {"Stop": [{"hooks": [1]}]}}"#,
                "hooks.Stop handlers must be objects",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err().to_string(), error);
        }
    }

    #[test]
    fn merge_matches_handlers_by_matcher_type_and_command() {
        let template = json!({"hooks": {"Stop": [
            {"matcher": "compact", "hooks": [{"type": "command", "command": "bbs", "timeout": 10}]}
        ]}});
        // A customized timeout still counts as installed.
        let custom = json!({"hooks": {"Stop": [
            {"matcher": "compact", "hooks": [{"type": "command", "command": "bbs", "timeout": 30}]}
        ]}});
        assert_eq!(merge_hooks(&custom, &template), custom);
        // A different matcher does not.
        let other = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "bbs", "timeout": 10}]}
        ]}});
        let merged = merge_hooks(&other, &template);
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }
}
