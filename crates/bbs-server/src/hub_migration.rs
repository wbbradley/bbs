//! `bbs-server migrate-hub-model`: the operator-only Hub model cutover.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::db;

/// Run the cutover, or with `dry_run` only its preflight, and return the
/// report to print. A real run needs `backup_taken`: an existing schema-26
/// database other than `db_path`.
pub fn run(
    db_path: &Path,
    namespace: &str,
    backup_taken: Option<&Path>,
    dry_run: bool,
) -> Result<String> {
    db::validate_namespace(namespace)?;
    if !db_path.is_file() {
        bail!("no database at {}", db_path.display());
    }
    if dry_run {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let report = db::hub_model_preflight(&conn, namespace)?;
        return Ok(preflight_text(&report));
    }
    let Some(backup) = backup_taken else {
        bail!("refusing to migrate without --backup-taken PATH naming a backup of this database");
    };
    check_backup(db_path, backup)?;
    // No create flag: a mistyped path must not become a new empty database.
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("opening {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let report = db::migrate_to_hub_model(&mut conn, namespace)?;
    let mut out = format!(
        "migrated {} to the Hub model (schema v{}) in namespace {namespace}\n\
         humans {}, agents {}, groups {}, messages {}, deliveries {}\n",
        db_path.display(),
        db::HUB_MODEL_SCHEMA_VERSION,
        report.humans,
        report.agents,
        report.groups,
        report.messages,
        report.deliveries,
    );
    mappings_text(&mut out, "renamed", &report.renamed);
    Ok(out)
}

/// A cheap guard that a backup exists: a readable schema-26 SQLite file that
/// is not the database being migrated.
fn check_backup(db_path: &Path, backup: &Path) -> Result<()> {
    let canonical = |p: &Path| {
        p.canonicalize()
            .with_context(|| format!("--backup-taken: cannot read {}", p.display()))
    };
    let backup_path = canonical(backup)?;
    if !backup_path.is_file() {
        bail!("--backup-taken: {} is not a file", backup.display());
    }
    if backup_path == canonical(db_path)? {
        bail!("--backup-taken names the database being migrated, not a backup");
    }
    let conn = Connection::open_with_flags(&backup_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("--backup-taken: opening {}", backup.display()))?;
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .with_context(|| {
            format!(
                "--backup-taken: {} is not a SQLite database",
                backup.display()
            )
        })?;
    if version != db::HUB_MODEL_SOURCE_VERSION as i64 {
        bail!(
            "--backup-taken: {} is at schema v{version}, not v{}",
            backup.display(),
            db::HUB_MODEL_SOURCE_VERSION
        );
    }
    Ok(())
}

fn mappings_text(out: &mut String, title: &str, mappings: &[db::EntityMapping]) {
    let _ = writeln!(out, "{title}: {}", mappings.len());
    for m in mappings {
        let _ = writeln!(
            out,
            "  {} {} {:?} -> {}{}",
            m.kind.as_str(),
            m.native_id,
            m.name,
            m.entity_id,
            if m.retired { " (retired)" } else { "" }
        );
    }
}

fn preflight_text(report: &db::PreflightReport) -> String {
    let mut out = format!(
        "Hub model preflight (dry run, nothing written), namespace {}\n\
         humans {}, agents {}, groups {}, messages {}, deliveries {}\n",
        report.namespace,
        report.count(db::HubKind::Human),
        report.count(db::HubKind::Agent),
        report.count(db::HubKind::Group),
        report.messages,
        report.deliveries,
    );
    mappings_text(&mut out, "renamed", &report.renamed());
    if report.is_blocked() {
        let _ = writeln!(out, "BLOCKED: {} issue(s)", report.issues.len());
        for issue in &report.issues {
            let _ = writeln!(out, "  - {issue}");
        }
    } else {
        out.push_str("ready: no blocking issues\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut conn = db::open(&path).unwrap();
        db::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO humans VALUES (1, 'will', 't', 't');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
                 VALUES (1, 'a.b', 'k', '/', 'h', 't', 't');
             INSERT INTO agent_owners VALUES (1, 1);
             INSERT INTO messages (id, from_kind, from_id, body, created_at)
                 VALUES (1, 'human', 1, 'hi', 't');
             INSERT INTO deliveries VALUES (1, 'agent', 1, NULL, 1);",
        )
        .unwrap();
        path
    }

    fn snapshot(path: &Path) -> Vec<String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn
            .prepare("SELECT name || ' ' || ifnull(sql, '') FROM sqlite_schema ORDER BY name")
            .unwrap();
        let mut out: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        out.push(format!("v{version}"));
        out
    }

    fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn refuses_without_a_valid_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = board(dir.path(), "bbs.db");
        let before = snapshot(&path);
        let err = run(&path, "will", None, false).unwrap_err().to_string();
        assert!(err.contains("--backup-taken"), "{err}");
        let missing = dir.path().join("missing.db");
        assert!(run(&path, "will", Some(&missing), false).is_err());
        let err = run(&path, "will", Some(&path), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a backup"), "{err}");
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not a database").unwrap();
        assert!(run(&path, "will", Some(&text), false).is_err());
        let stale = dir.path().join("stale.db");
        Connection::open(&stale)
            .unwrap()
            .pragma_update(None, "user_version", 25)
            .unwrap();
        let err = run(&path, "will", Some(&stale), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("schema v25, not v26"), "{err}");
        assert_eq!(snapshot(&path), before);
        // A missing database is never created.
        assert!(run(&missing, "will", Some(&stale), false).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = board(dir.path(), "bbs.db");
        let before = snapshot(&path);
        let bytes = std::fs::read(&path).unwrap();
        let out = run(&path, "will", None, true).unwrap();
        assert!(out.contains("agent 1 \"a.b\" -> will/a-b"), "{out}");
        assert!(out.contains("ready: no blocking issues"), "{out}");
        assert_eq!(snapshot(&path), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn migrates_with_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = board(dir.path(), "bbs.db");
        let backup = dir.path().join("backup.db");
        db::backup(&path, &backup).unwrap();
        let out = run(&path, "will", Some(&backup), false).unwrap();
        assert!(out.contains("messages 1, deliveries 1"), "{out}");
        assert_eq!(version(&path), 27);
        assert_eq!(version(&backup), 26);
        let err = db::migrate(&mut db::open(&path).unwrap()).unwrap_err();
        assert!(err.to_string().contains("migrated to the Hub model"));
    }
}
