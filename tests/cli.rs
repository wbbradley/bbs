use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

/// Which backend the CLI under test uses.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// The CLI opens ~/.local/state/bbs/bbs.db itself.
    Local,
    /// The CLI talks to a `bbs serve` spawned for the test, via `BBS_SERVER`.
    Remote,
}

/// Generate `local::<test>` and `remote::<test>` for each `fn <test>(mode: Mode)`.
macro_rules! both_modes {
    ($($name:ident),* $(,)?) => {
        mod local {
            $(#[test] fn $name() { super::$name(super::Mode::Local) })*
        }
        mod remote {
            $(#[test] fn $name() { super::$name(super::Mode::Remote) })*
        }
    };
}

struct Server {
    child: Child,
    addr: String,
    state: PathBuf,
}

impl Server {
    fn start(home: &PathBuf) -> Server {
        let state = home.join("server-state");
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--state-dir"])
            .arg(&state)
            .env("HOME", home)
            .env_remove("BBS_SERVER")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .strip_prefix("bbs serve listening on ")
            .unwrap_or_else(|| panic!("unexpected serve output: {line:?}"))
            .to_string();
        Server { child, addr, state }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Bbs {
    home: PathBuf,
    server: Option<Server>,
}

impl Bbs {
    fn new(test: &str) -> Bbs {
        Bbs::with_mode(test, Mode::Local)
    }

    fn with_mode(test: &str, mode: Mode) -> Bbs {
        let home =
            std::env::temp_dir().join(format!("bbs-test-{}-{test}-{mode:?}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let server = match mode {
            Mode::Local => None,
            Mode::Remote => Some(Server::start(&home)),
        };
        let bbs = Bbs { home, server };
        bbs.ok(&["init"]);
        bbs
    }

    /// A `bbs` invocation as the human "will", pointed at this test's backend.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bbs"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("USER", "will")
            .env_remove("BBS_SERVER");
        if let Some(server) = &self.server {
            cmd.env("BBS_SERVER", &server.addr);
        }
        cmd
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        self.run_with_env(args, stdin, &[])
    }

    fn run_with_env(&self, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
        let mut child = self
            .command(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, "")
    }

    /// Let `agent` broadcast to `group`, as a human.
    fn grant_write(&self, group: &str, agent: &str) {
        self.ok(&["group", "grant", group, agent]);
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "bbs {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "bbs {args:?} unexpectedly succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    /// Register an agent and return (name, token).
    fn register(&self) -> (String, String) {
        self.register_with(&["register", "--note", "testing"])
    }

    fn register_with(&self, args: &[&str]) -> (String, String) {
        let out = self.ok(args);
        let name = out.split('"').nth(1).unwrap().to_string();
        let token = out
            .split("--token ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(token.len(), 26, "{out}");
        (name, token)
    }

    /// The backend's database: the server's in remote mode.
    fn db(&self) -> rusqlite::Connection {
        let state = match &self.server {
            Some(server) => server.state.clone(),
            None => self.home.join(".local/state/bbs"),
        };
        rusqlite::Connection::open(state.join("bbs.db")).unwrap()
    }
}

impl Drop for Bbs {
    fn drop(&mut self) {
        // Stop the server before removing its state directory.
        self.server = None;
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn bare_bbs_outside_a_terminal_prints_usage_without_side_effects() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-bare", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .env("HOME", &home)
        .env("BBS_SERVER", "127.0.0.1:1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Usage:"), "{err}");
    assert!(
        !home.join("bbs.log").exists(),
        "no log file: {}",
        home.display()
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn requires_init() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-noinit", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["read"])
        .env("HOME", &home)
        .env_remove("BBS_SERVER")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bbs init"));
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn version_reports_identity_without_side_effects() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-version", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    for json in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bbs"));
        command.arg("version");
        if json {
            command.arg("--json");
        }
        // An unreachable server would fail any command that tried to connect.
        let out = command
            .env("HOME", &home)
            .env("BBS_SERVER", "127.0.0.1:1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        if json {
            let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
            let revision = report["revision"].as_str().unwrap();
            assert!(revision == "unknown" || revision.len() == 40, "{revision}");
            assert!(
                ["clean", "dirty", "unknown"].contains(&report["provenance"].as_str().unwrap())
            );
            assert_eq!(report["protocol"]["epoch"], 2);
            assert!(report["protocol"]["capabilities"].as_array().unwrap().len() >= 4);
            assert_eq!(report["schema_version"], bbs_core::db::SCHEMA_VERSION);
            assert_eq!(report["config_format"], 4);
            assert!(!report["target"].as_str().unwrap().is_empty());
        } else {
            assert!(stdout.starts_with(&format!("bbs {} (", env!("CARGO_PKG_VERSION"))));
        }
    }
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn version_manifest_matches_identity_only_for_clean_builds() {
    let run = |arg| {
        Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["version", arg])
            .output()
            .unwrap()
    };
    let identity: serde_json::Value = serde_json::from_slice(&run("--json").stdout).unwrap();
    let out = run("--manifest");
    if identity["provenance"] != "clean" {
        assert!(!out.status.success(), "{out:?}");
        assert!(out.stdout.is_empty());
        return;
    }
    assert!(out.status.success(), "{out:?}");
    let manifest: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(manifest["format"], 1);
    assert_eq!(
        manifest["tag"],
        format!("v{}", identity["version"].as_str().unwrap())
    );
    for key in [
        "version",
        "revision",
        "protocol",
        "schema_version",
        "config_format",
    ] {
        assert_eq!(manifest[key], identity[key], "{key}");
    }
}

fn init_is_idempotent(mode: Mode) {
    let bbs = Bbs::with_mode("reinit", mode);
    bbs.ok(&["group", "register", "acme"]);
    let (_, token) = bbs.register();

    let out = bbs.ok(&["init"]);
    assert!(out.contains("Human: will"), "{out}");

    let db = bbs.db();
    let humans: i64 = db
        .query_row("SELECT COUNT(*) FROM humans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(humans, 1);
    assert!(bbs.ok(&["group", "list"]).contains("acme"));
    bbs.ok(&["--token", &token, "group", "join", "acme"]);
}

fn requested_names_are_validated_and_reserved(mode: Mode) {
    let bbs = Bbs::with_mode("requested-name", mode);
    let (name, token) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    assert_eq!(name, "Jeffrey");
    assert_eq!(bbs.ok(&["--token", &token, "whoami"]), "Jeffrey\n");
    assert!(
        bbs.err(&["register", "--name", "jEfFrEy"])
            .contains("already reserved")
    );
    assert!(
        bbs.err(&["register", "--name", "WILL"])
            .contains("already reserved")
    );
    assert!(
        bbs.err(&["human", "add", "Jeffrey"])
            .contains("in use by an agent")
    );
    assert!(
        bbs.err(&["--as", "missing", "register"])
            .contains("unknown human")
    );
    for invalid in [
        "",
        "@Jeffrey",
        "Jeffrey@will",
        "two words",
        "a,b",
        "../x",
        "-a",
        "é",
        &"a".repeat(65),
    ] {
        assert!(
            bbs.err(&["register", &format!("--name={invalid}")])
                .contains("invalid agent name")
        );
    }
    let count: i64 = bbs
        .db()
        .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "failed requests must not register fallback identities"
    );
    bbs.ok(&["--token", &token, "bye"]);
    let (name, replacement) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    assert_eq!(name, "Jeffrey");
    assert_ne!(replacement, token);
    bbs.register_with(&["register", "--name", "Agent_1.test-2"]);
}

fn concurrent_registration_reserves_one_name(mode: Mode) {
    let bbs = Bbs::with_mode("name-race", mode);
    let children: Vec<_> = (0..6)
        .map(|_| bbs.spawn(&["register", "--name", "Jeffrey"]))
        .collect();
    let mut successes = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            successes += 1;
        } else {
            let err = String::from_utf8(out.stderr).unwrap();
            assert!(err.contains("already reserved"), "{err}");
        }
    }
    assert_eq!(successes, 1);
}

fn registration_records_human_from_as_config_or_user(mode: Mode) {
    let bbs = Bbs::with_mode("owner-config", mode);
    let (_, user) = bbs.register();
    bbs.ok(&["init", "--human", "Ada"]);
    let (_, configured) = bbs.register();
    let (_, explicit) = bbs.register_with(&["--as", "WILL", "register"]);
    let db = bbs.db();
    for (token, expected) in [(&user, "will"), (&configured, "Ada"), (&explicit, "will")] {
        let owner: String = db
            .query_row(
                "SELECT h.name FROM agents a JOIN agent_owners o ON o.agent_id = a.id
             JOIN humans h ON h.id = o.human_id WHERE a.token = ?1",
                [token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, expected);
    }
    assert!(
        db.execute(
            "INSERT INTO agent_owners (agent_id, human_id)
         SELECT a.id, h.id FROM agents a, humans h WHERE a.token = ?1 AND h.name = 'Ada'",
            [&user],
        )
        .is_err(),
        "an agent may have only one owner"
    );
}

fn cross_owner_messages_are_qualified_for_each_reader(mode: Mode) {
    let bbs = Bbs::with_mode("cross-owner", mode);
    bbs.ok(&["human", "add", "Ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (sender, ts) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    let (same, same_token) = bbs.register();
    let (other, other_token) = bbs.register_with(&["--as", "ada", "register"]);
    for token in [&ts, &same_token, &other_token] {
        bbs.ok(&["--token", token, "group", "join", "acme"]);
    }
    bbs.grant_write("acme", &sender);
    for token in [&ts, &same_token, &other_token] {
        bbs.ok(&["--token", token, "read"]);
    }
    bbs.ok(&["--token", &ts, "send", "--to", "@acme", "broadcast"]);
    let same_mail = bbs.ok(&["--token", &same_token, "read"]);
    let other_mail = bbs.ok(&["--token", &other_token, "read"]);
    assert!(same_mail.contains("from Jeffrey to @acme:"), "{same_mail}");
    assert!(
        other_mail.contains("from Jeffrey@will to @acme:"),
        "{other_mail}"
    );
    assert!(bbs.ok(&["read"]).contains("from Jeffrey to @acme:"));

    bbs.ok(&["--token", &ts, "send", "--to", &other, "direct"]);
    let direct = bbs.ok(&["--token", &other_token, "read", "--wait"]);
    assert!(
        direct.contains(&format!("from Jeffrey@will to {other}:\ndirect")),
        "{direct}"
    );
    bbs.ok(&[
        "--token",
        &other_token,
        "send",
        "--to",
        "jeffrey@WILL",
        "reply",
    ]);
    let reply = bbs.ok(&["--token", &ts, "read"]);
    assert!(
        reply.contains(&format!("from {other}@Ada to Jeffrey:")),
        "{reply}"
    );
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@Ada",
            "wrong owner"
        ])
        .contains("no such")
    );
    bbs.ok(&["--token", &ts, "send", "--to", &same, "same owner"]);
    assert!(
        bbs.ok(&["--token", &same_token, "read"])
            .contains("from Jeffrey to ")
    );

    bbs.ok(&["--token", &ts, "bye"]);
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@will",
            "retired"
        ])
        .contains("offline")
    );
    let (_, replacement) = bbs.register_with(&["--as", "Ada", "register", "--name", &sender]);
    bbs.ok(&["--token", &replacement, "group", "join", "acme"]);
    bbs.ok(&["--token", &replacement, "send", "--to", &other, "new owner"]);
    assert!(
        bbs.ok(&["--token", &other_token, "read"])
            .contains(&format!("from Jeffrey to {other}:\nnew owner"))
    );
    let history = bbs.ok(&["--token", &other_token, "read", "--archived"]);
    assert!(
        history.contains(&format!("from Jeffrey@will to {other}:\ndirect")),
        "{history}"
    );
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@will",
            "stale address"
        ])
        .contains("offline")
    );
}

#[test]
fn online_backup_preserves_committed_state_and_restores() {
    let bbs = Bbs::with_mode("backup", Mode::Remote);
    let (name, token) = bbs.register();
    bbs.ok(&["send", "--to", &name, "saved message"]);
    let state = &bbs.server.as_ref().unwrap().state;
    let destination = bbs.home.join("snapshot.db");
    let mut source = bbs.db();
    let transaction = source.transaction().unwrap();
    transaction
        .execute(
            "UPDATE humans SET name = 'uncommitted' WHERE name = 'will'",
            [],
        )
        .unwrap();
    let output = bbs
        .command(&[
            "backup",
            destination.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .env("BBS_SERVER", "127.0.0.1:1")
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    transaction.rollback().unwrap();
    let snapshot = rusqlite::Connection::open(&destination).unwrap();
    let human: String = snapshot
        .query_row("SELECT name FROM humans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        human, "will",
        "uncommitted changes must not enter the backup"
    );
    let integrity: String = snapshot
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let journal: String = snapshot
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete", "backup must be a standalone file");
    drop(snapshot);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!destination.with_file_name("snapshot.db-wal").exists());
    // The live server still works; changes after the snapshot stay out of it.
    assert!(
        bbs.ok(&["--token", &token, "read"])
            .contains("saved message")
    );
    bbs.ok(&["send", "--to", &name, "after backup"]);
    let restored_home = bbs.home.join("restored");
    std::fs::create_dir_all(restored_home.join("server-state")).unwrap();
    std::fs::copy(&destination, restored_home.join("server-state/bbs.db")).unwrap();
    let restored = Bbs {
        server: Some(Server::start(&restored_home)),
        home: restored_home,
    };
    let mail = restored.ok(&["--token", &token, "read"]);
    assert!(mail.contains("saved message"));
    assert!(!mail.contains("after backup"));
}

#[test]
fn backup_refuses_existing_files_and_cleans_up_failures() {
    let bbs = Bbs::new("backup-errors");
    let destination = bbs.home.join("snapshot.db");
    std::fs::write(&destination, "keep me").unwrap();
    assert!(
        bbs.err(&["backup", destination.to_str().unwrap()])
            .contains("already exists")
    );
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "keep me");
    std::fs::remove_file(&destination).unwrap();
    let missing = bbs.home.join("missing-state");
    assert!(
        bbs.err(&[
            "backup",
            destination.to_str().unwrap(),
            "--state-dir",
            missing.to_str().unwrap()
        ])
        .contains("backup source")
    );
    assert!(!destination.exists());
    assert!(!missing.exists());
    let sidecar = bbs.home.join("snapshot.db-wal");
    std::fs::write(&sidecar, "keep me").unwrap();
    assert!(
        bbs.err(&["backup", destination.to_str().unwrap()])
            .contains("sidecar")
    );
    assert_eq!(std::fs::read_to_string(sidecar).unwrap(), "keep me");
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(&bbs.home).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp")
    }));
}

#[test]
fn backup_pins_snapshot_across_copy_steps() {
    let bbs = Bbs::with_mode("backup-snapshot", Mode::Remote);
    let (name, _) = bbs.register();
    bbs.ok(&["send", "--to", &name, "before copy"]);
    bbs.db()
        .execute_batch(
            "CREATE TABLE padding (data BLOB); INSERT INTO padding VALUES (zeroblob(16777216))",
        )
        .unwrap();
    let destination = bbs.home.join("snapshot.db");
    let child = bbs.spawn(&[
        "backup",
        destination.to_str().unwrap(),
        "--state-dir",
        bbs.server.as_ref().unwrap().state.to_str().unwrap(),
    ]);
    // A partly written temporary database establishes that copying has begun,
    // without relying on a sleep to guess when the snapshot was acquired.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let copying = std::fs::read_dir(&bbs.home).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry.file_name().to_string_lossy().starts_with(".tmp")
                && !entry.file_name().to_string_lossy().ends_with("-journal")
                && entry.metadata().unwrap().len() > 1024 * 1024
        });
        if copying {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backup did not begin copying pages"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    bbs.ok(&["send", "--to", &name, "during copy"]);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snapshot = rusqlite::Connection::open(&destination).unwrap();
    let count: i64 = snapshot
        .query_row(
            "SELECT count(*) FROM messages WHERE body = 'during copy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "writes during copying must not restart the snapshot"
    );
    let count: i64 = snapshot
        .query_row(
            "SELECT count(*) FROM messages WHERE body = 'before copy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn backup_refuses_source_sidecar_names() {
    let bbs = Bbs::new("backup-sidecar");
    let state = bbs.home.join(".local/state/bbs");
    for suffix in ["-wal", "-shm", "-journal"] {
        let destination = state.join(format!("bbs.db{suffix}"));
        let error = bbs.err(&["backup", destination.to_str().unwrap()]);
        assert!(
            error.contains("sidecar") || error.contains("already exists"),
            "{error}"
        );
    }
    #[cfg(unix)]
    {
        let alias = bbs.home.join("state-alias");
        std::os::unix::fs::symlink(&state, &alias).unwrap();
        let destination = alias.join("bbs.db-journal");
        assert!(
            bbs.err(&["backup", destination.to_str().unwrap()])
                .contains("source database sidecar")
        );
    }
}

fn config_path(bbs: &Bbs) -> PathBuf {
    bbs.home.join(".config/bbs/config.toml")
}

#[test]
fn init_writes_and_merges_config() {
    let bbs = Bbs::new("config");
    assert!(!config_path(&bbs).exists(), "plain init writes no config");
    // Bind a session, so a compaction hook would re-inject its identity.
    let register_out = bbs.ok(&["register"]);
    hook(
        &bbs,
        serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "bbs register" },
            "tool_response": { "stdout": register_out, "stderr": "" },
        }),
    );

    // Failed enrollment preserves the working local configuration.
    let down = "connecting to bbs server at 127.0.0.1:1";
    assert!(bbs.err(&["init", "--server", "127.0.0.1:1"]).contains(down));
    assert!(!config_path(&bbs).exists());
    bbs.ok(&["init", "--human", "ada"]);
    let config = std::fs::read_to_string(config_path(&bbs)).unwrap();
    assert!(config.contains("human = \"ada\""), "{config}");
    assert!(!config.contains("server"));
    std::fs::write(
        config_path(&bbs),
        format!("{config}server = \"127.0.0.1:1\"\n"),
    )
    .unwrap();

    // Commands use the configured server; hooks stay silent when it is down.
    assert!(bbs.err(&["whoami"]).contains(down));
    let compact = serde_json::json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "compact"});
    assert_eq!(hook(&bbs, compact.clone()), None);
    std::fs::remove_file(config_path(&bbs)).unwrap();
    assert!(hook(&bbs, compact).is_some());
}

#[test]
fn human_name_comes_from_as_then_config_then_user() {
    let bbs = Bbs::new("human-config");
    // Local init with --human saves the name and reserves it.
    let out = bbs.ok(&["init", "--human", "ada"]);
    assert!(out.contains("Human: ada"), "{out}");
    assert_eq!(bbs.ok(&["whoami"]), "ada\n");
    assert_eq!(bbs.ok(&["--as", "will", "whoami"]), "will\n");
    std::fs::remove_file(config_path(&bbs)).unwrap();
    assert_eq!(bbs.ok(&["whoami"]), "will\n");
}

#[test]
fn bbs_server_env_overrides_config() {
    let bbs = Bbs::new("server-env");
    let out = bbs.run_with_env(&["whoami"], "", &[("BBS_SERVER", "127.0.0.1:1")]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("connecting to bbs server at 127.0.0.1:1")
    );
    assert!(!config_path(&bbs).exists());
}

#[test]
fn remote_backend_round_trip() {
    let bbs = Bbs::new("remote");
    let server = Server::start(&bbs.home);
    let out = bbs.ok(&["init", "--server", &server.addr]);
    assert!(out.contains("Human: will"), "{out}");

    bbs.ok(&["group", "register", "acme"]);
    let register_out = bbs.ok(&["register"]);
    let (a, ta) = bbs.register();
    let b = register_out.split('"').nth(1).unwrap();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.grant_write("acme", &a);
    bbs.ok(&["--token", &ta, "read"]);
    bbs.ok(&["--token", &ta, "send", "--to", "will", "hi"]);
    assert!(
        bbs.ok(&["read"])
            .contains(&format!("from {a} to will:\nhi"))
    );
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(bbs.ok(&["read"]).contains("standup"));
    assert_eq!(bbs.ok(&["whoami"]), "will\n");

    // Hooks bind through the server but leave mail for the listener.
    let post_tool = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "bbs register" },
        "tool_response": { "stdout": register_out, "stderr": "" },
    });
    assert!(hook(&bbs, post_tool).is_none());
    bbs.ok(&["send", "--to", b, "from will"]);
    let compact = serde_json::json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": "compact"});
    let out = hook(&bbs, compact).unwrap();
    let tb = register_out.split("--token ").nth(1).unwrap();
    let tb = tb.split(' ').next().unwrap();
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(tb),
        "{out}"
    );
    for name in ["UserPromptSubmit", "Stop"] {
        assert!(
            hook(
                &bbs,
                serde_json::json!({"session_id": "s1", "hook_event_name": name})
            )
            .is_none()
        );
    }
    assert!(bbs.ok(&["--token", tb, "read"]).contains("from will"));

    // `read --wait` blocks on the server until mail arrives.
    let mut waiter = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        bbs.err(&["--token", &ta, "read", "--wait"])
            .contains("already running")
    );
    bbs.ok(&["send", "--to", &a, "wake up"]);
    assert!(wait_exit(&mut waiter, 5).unwrap().success());
    let out = waiter.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("wake up"));

    // Everything went to the server, not the database from the local `init`.
    let local_agents: i64 = bbs
        .db()
        .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(local_agents, 0);
}

fn direct_messages_within_group(mode: Mode) {
    let bbs = Bbs::with_mode("dm", mode);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    assert_ne!(a, b);

    // Not sharing a group yet.
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", &b, "hi"])
            .contains("not in any of your groups")
    );

    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    bbs.ok(&["--token", &ta, "send", "--to", &b, "hello there"]);

    let out = bbs.ok(&["--token", &tb, "read"]);
    assert!(
        out.contains(&format!("from {a} to {b}:\nhello there\n")),
        "{out}"
    );
    // Archived after reading: empty stdout, exit 0.
    assert_eq!(bbs.ok(&["--token", &tb, "read"]), "");
    let out = bbs.ok(&["--token", &tb, "read", "--archived", "--since", "1h"]);
    assert!(out.contains("hello there"), "{out}");
}

fn humans_and_agents_message_each_other(mode: Mode) {
    let bbs = Bbs::with_mode("human", mode);
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    // Any human, with no shared group; agents see humans to address.
    bbs.ok(&["--token", &ta, "send", "--to", "alice", "need input"]);
    assert!(
        bbs.ok(&["--as", "alice", "read"])
            .contains(&format!("from {a} to alice:\nneed input"))
    );
    assert!(bbs.ok(&["--token", &ta, "agents"]).contains("alice"));
    bbs.ok(&["send", "--to", "alice", "lunch?"]);
    assert!(
        bbs.ok(&["--as", "alice", "read"])
            .contains("from will (human) to alice:\nlunch?")
    );

    assert!(bbs.err(&["send", "--to", &a, "-"]).contains("empty"));
    let out = bbs.run_with_stdin(&["send", "--to", &a], "multi\nline\n");
    assert!(out.status.success());
    let out = bbs.ok(&["--token", &ta, "read"]);
    assert!(out.contains("from will (human)"), "{out}");
    assert!(out.contains("multi\nline\n"), "{out}");
}

fn broadcast_reaches_members_and_humans(mode: Mode) {
    let bbs = Bbs::with_mode("broadcast", mode);
    bbs.ok(&["group", "register", "acme"]);
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    let (_b, tb) = bbs.register();
    let (_c, tc) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);

    assert!(
        bbs.err(&["--token", &tc, "send", "--to", "@acme", "x"])
            .contains("not a member")
    );
    // Members broadcast only once a human grants them write.
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "x"])
            .contains(
                "you lack write access to @acme; only a human can grant it. @acme has no agent writers."
            )
    );
    bbs.grant_write("acme", &a);
    assert!(
        bbs.err(&["--token", &tb, "send", "--to", "@acme", "x"]).contains(&format!(
            "you lack write access to @acme; only a human can grant it. Writers: {a} (message one of them to reach the group)."
        ))
    );
    let out = bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(out.contains("1 recipients"), "{out}"); // humans are not counted for agents

    assert!(
        bbs.ok(&["--token", &tb, "read"])
            .contains(&format!("from {a} to @acme:\nstandup"))
    );
    // A has only B's join notice and the grant, not its own broadcast.
    assert!(!bbs.ok(&["--token", &ta, "read"]).contains("standup"));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert!(bbs.ok(&["--as", "alice", "read"]).contains("standup"));

    let out = bbs.ok(&["send", "--to", "@acme", "from a human"]);
    assert!(out.contains("3 recipients"), "{out}"); // both agents + alice
    assert!(
        bbs.ok(&["--token", &ta, "read"])
            .contains("from will (human) to @acme")
    );
}

fn bye_retires_token_and_name(mode: Mode) {
    let bbs = Bbs::with_mode("bye", mode);
    let (a, ta) = bbs.register();
    bbs.ok(&["send", "--to", &a, "unread"]);
    bbs.ok(&["--token", &ta, "bye"]);
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    assert!(bbs.err(&["send", "--to", &a, "hi"]).contains("offline"));
    assert!(!bbs.ok(&["agents"]).contains(&a));
    assert!(bbs.ok(&["agents", "--all"]).contains("retired"));
}

fn bye_notifies_and_leaves_groups(mode: Mode) {
    let bbs = Bbs::with_mode("bye-groups", mode);
    bbs.ok(&["group", "register", "g"]);
    bbs.ok(&["group", "register", "h"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, tc) = bbs.register();
    for g in ["g", "h"] {
        bbs.ok(&["--token", &ta, "group", "join", g]);
    }
    bbs.ok(&["--token", &tb, "group", "join", "g"]);
    bbs.ok(&["--token", &tc, "group", "join", "h"]);
    for t in [&tb, &tc] {
        bbs.ok(&["--token", t, "read"]);
    }
    bbs.ok(&["read"]);

    bbs.ok(&["--token", &ta, "bye"]);
    let from_g = format!(
        "from {a} to @g:\nAgent {a:?} retired. Agent {a:?} is no longer a member of group @g.\n"
    );
    let from_h = format!(
        "from {a} to @h:\nAgent {a:?} retired. Agent {a:?} is no longer a member of group @h.\n"
    );
    let mail_b = bbs.ok(&["--token", &tb, "read"]);
    assert!(
        mail_b.contains(&from_g) && !mail_b.contains("@h"),
        "{mail_b}"
    );
    let mail_c = bbs.ok(&["--token", &tc, "read"]);
    assert!(
        mail_c.contains(&from_h) && !mail_c.contains("@g"),
        "{mail_c}"
    );
    let human = bbs.ok(&["read"]);
    assert!(
        human.contains(&from_g) && human.contains(&from_h),
        "{human}"
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@g: {b}\n@h: {c}\n"));
}

fn humans_retire_agents(mode: Mode) {
    let bbs = Bbs::with_mode("retire", mode);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "g"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    for t in [&ta, &tb] {
        bbs.ok(&["--token", t, "group", "join", "g"]);
    }
    bbs.ok(&["--token", &ta, "read"]);
    bbs.ok(&["--as", "ada", "read"]);

    assert!(
        bbs.err(&["--token", &tb, "retire", &a])
            .contains("humans only")
    );
    assert_eq!(bbs.ok(&["retire", &a]), format!("Retired {a}.\n"));
    let notice = format!(
        "from will (human) to @g:\nHuman \"will\" retired agent {a:?}. Agent {a:?} is no longer a member of group @g.\n"
    );
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&notice));
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&notice));
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@g: {b}\n"));
    assert!(bbs.err(&["retire", &a]).contains("no live agent"));
}

fn stale_agents_are_retired_on_register(mode: Mode) {
    let bbs = Bbs::with_mode("stale", mode);
    let (a, ta) = bbs.register();
    bbs.ok(&["send", "--to", &a, "old news"]);
    bbs.db()
        .execute(
            "UPDATE agents SET active_at = '2000-01-01T00:00:00.000000Z'",
            [],
        )
        .unwrap();
    bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    let unread: i64 = bbs
        .db()
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE archived_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unread, 0);
}

fn human_name_is_reserved_and_permissions_enforced(mode: Mode) {
    let bbs = Bbs::with_mode("perms", mode);
    let (_a, ta) = bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "group", "register", "x"])
            .contains("humans only")
    );
    assert!(
        bbs.err(&["group", "join", "x"])
            .contains("requires --token")
    );
    assert!(
        bbs.err(&["--as", "nobody", "read"])
            .contains("unknown human")
    );
    let taken: i64 = bbs
        .db()
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE lower(name) = 'will'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(taken, 0);
}

fn whoami_reports_agent_or_human(mode: Mode) {
    let bbs = Bbs::with_mode("whoami", mode);
    let (a, ta) = bbs.register();
    assert_eq!(bbs.ok(&["--token", &ta, "whoami"]), format!("{a}\n"));
    assert_eq!(bbs.ok(&["whoami"]), "will\n");
    bbs.ok(&["human", "add", "ada"]);
    assert_eq!(bbs.ok(&["--as", "ada", "whoami"]), "ada\n");
    assert!(
        bbs.err(&["--as", "nobody", "whoami"])
            .contains("unknown human")
    );
    bbs.ok(&["--token", &ta, "bye"]);
    assert!(
        bbs.err(&["--token", &ta, "whoami"])
            .contains("unknown or retired token")
    );
}

fn humans_remove_agents_from_groups(mode: Mode) {
    let bbs = Bbs::with_mode("remove", mode);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);

    assert!(
        bbs.err(&["--token", &tb, "group", "remove", "acme", &a])
            .contains("humans only")
    );
    assert_eq!(
        bbs.ok(&["group", "remove", "@acme", &a.to_lowercase()]),
        format!("Removed {a} from @acme.\n")
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@acme: {b}\n"));
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "hi"])
            .contains("not a member")
    );
    assert!(
        bbs.err(&["group", "remove", "acme", &a])
            .contains(&format!("{a} is not in @acme"))
    );
    assert!(
        bbs.err(&["group", "remove", "acme", "Nobody"])
            .contains("no live agent")
    );
    assert!(
        bbs.err(&["group", "remove", "nope", &b])
            .contains("no such group")
    );
}

fn deleting_groups_archives_history_and_removes_access(mode: Mode) {
    let bbs = Bbs::with_mode("archive-group", mode);
    bbs.ok(&["group", "register", "acme"]);
    bbs.ok(&["group", "register", "other"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    for token in [&ta, &tb] {
        bbs.ok(&["--token", token, "group", "join", "acme"]);
    }
    bbs.grant_write("acme", &a);
    for token in [&ta, &tb] {
        bbs.ok(&["--token", token, "read"]);
    }
    bbs.ok(&["--token", &ta, "group", "join", "other"]);
    bbs.ok(&[
        "--token",
        &ta,
        "send",
        "--to",
        "@acme",
        "retained broadcast",
    ]);
    assert!(
        bbs.err(&["--token", &ta, "group", "delete", "acme"])
            .contains("humans only")
    );
    assert!(
        bbs.err(&["group", "delete", "missing"])
            .contains("no such group")
    );
    assert_eq!(
        bbs.ok(&["group", "delete", "@acme"]),
        "Archived group @acme.\n"
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@other: {a}\n"));
    assert_eq!(
        bbs.ok(&["--token", &ta, "group", "list"]),
        format!("@other: {a}\n")
    );
    assert_eq!(bbs.ok(&["--token", &tb, "group", "list"]), "");
    assert!(!bbs.ok(&["agents"]).contains("acme"));
    assert!(!bbs.ok(&["--token", &ta, "agents"]).contains(&b));
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", &b, "denied"])
            .contains("not in any of your groups")
    );
    for command in ["join", "leave"] {
        assert!(
            bbs.err(&["--token", &ta, "group", command, "acme"])
                .contains("no such group")
        );
    }
    assert!(
        bbs.err(&["group", "remove", "acme", &a])
            .contains("no such group")
    );
    assert!(
        bbs.err(&["send", "--to", "@acme", "denied"])
            .contains("no such group")
    );
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "denied"])
            .contains("no such group")
    );
    assert!(
        bbs.err(&["group", "delete", "acme"])
            .contains("no such group")
    );

    // Archived membership rows cannot authorize access, but history remains readable.
    let db = bbs.db();
    let (archived, members): (bool, i64) = db.query_row(
        "SELECT archived_at IS NOT NULL, (SELECT COUNT(*) FROM group_members WHERE group_id = g.id)
         FROM bbs_groups g WHERE name = 'acme'", [], |r| Ok((r.get(0)?, r.get(1)?))
    ).unwrap();
    assert!(archived);
    assert_eq!(members, 2);
    let notice = "Human \"will\" archived group @acme.";
    let unread = bbs.ok(&["--token", &tb, "read"]);
    assert!(unread.contains("to @acme:\nretained broadcast"), "{unread}");
    assert_eq!(unread.matches(notice).count(), 1, "{unread}");
    let history = bbs.ok(&["--token", &tb, "read", "--archived"]);
    assert!(history.contains("to @acme:\nretained broadcast"));
    assert!(history.contains(notice));
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(notice));

    // Sharing another active group still grants access independently.
    bbs.ok(&["--token", &tb, "group", "join", "other"]);
    bbs.ok(&["--token", &ta, "send", "--to", &b, "allowed"]);
    assert!(bbs.ok(&["--token", &tb, "read"]).contains("allowed"));
    bbs.ok(&["read"]);
    bbs.ok(&["--token", &ta, "bye"]);
    let retirement = bbs.ok(&["read"]);
    assert!(retirement.contains("@other"));
    assert!(!retirement.contains("@acme"), "{retirement}");

    // Reusing a name creates a new scope and never restores old membership.
    bbs.ok(&["group", "register", "acme"]);
    assert!(
        bbs.err(&["group", "register", "acme"])
            .contains("already exists")
    );
    assert!(!bbs.ok(&["--token", &tb, "group", "list"]).contains("acme"));
    assert!(
        bbs.err(&["--token", &tb, "send", "--to", "@acme", "denied"])
            .contains("not a member")
    );
    let (c, tc) = bbs.register();
    bbs.ok(&["--token", &tc, "group", "join", "acme"]);
    bbs.ok(&["send", "--to", "@acme", "new broadcast"]);
    assert!(bbs.ok(&["--token", &tc, "read"]).contains("new broadcast"));
    assert!(!bbs.ok(&["--token", &tb, "read"]).contains("new broadcast"));
    assert!(
        !bbs.ok(&["--token", &tc, "read", "--archived"])
            .contains("retained broadcast")
    );
    assert!(
        bbs.err(&["--token", &tb, "send", "--to", &c, "denied"])
            .contains("not in any of your groups")
    );
    assert!(
        bbs.ok(&["--token", &tb, "read", "--archived"])
            .contains("retained broadcast")
    );
    let (old_id, new_id): (i64, i64) = db
        .query_row(
            "SELECT min(id), max(id) FROM bbs_groups WHERE name = 'acme'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_ne!(old_id, new_id);
    bbs.ok(&["group", "delete", "acme"]);
    bbs.ok(&["group", "register", "acme"]);
    assert!(bbs.ok(&["group", "list"]).contains("@acme: (no agents)\n"));
}

fn group_archive_rolls_back_notice_on_failure(mode: Mode) {
    let bbs = Bbs::with_mode("archive-group-rollback", mode);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["read"]);
    let db = bbs.db();
    db.execute_batch(
        "CREATE TRIGGER reject_archive BEFORE UPDATE OF archived_at ON bbs_groups
         BEGIN SELECT RAISE(ABORT, 'archive blocked'); END;",
    )
    .unwrap();
    assert!(
        bbs.err(&["group", "delete", "acme"])
            .contains("archive blocked")
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@acme: {a}\n"));
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
    assert_eq!(bbs.ok(&["read"]), "");
    db.execute_batch("DROP TRIGGER reject_archive").unwrap();
    bbs.ok(&["group", "delete", "acme"]);
    assert!(
        bbs.ok(&["--token", &ta, "read"])
            .contains("archived group @acme")
    );
    bbs.ok(&["group", "register", "empty"]);
    bbs.ok(&["group", "delete", "empty"]);
    assert_eq!(bbs.ok(&["group", "list"]), "");
}

fn groups_carry_human_written_descriptions(mode: Mode) {
    let bbs = Bbs::with_mode("group-description", mode);
    assert_eq!(
        bbs.ok(&[
            "group",
            "register",
            "infra",
            "--description",
            "  GPU fleet and deploys  "
        ]),
        "Registered group @infra.\n"
    );
    bbs.ok(&["group", "register", "plain"]);
    let (a, ta) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "infra"]);
    bbs.ok(&["read"]);
    bbs.ok(&["--token", &ta, "read"]);

    // Stored trimmed, shown indented under its group; groups without one are unchanged.
    let listing = format!("@infra: {a}\n  GPU fleet and deploys\n@plain: (no agents)\n");
    assert_eq!(bbs.ok(&["group", "list"]), listing);
    assert_eq!(
        bbs.ok(&["--token", &ta, "group", "list"]),
        format!("@infra: {a}\n  GPU fleet and deploys\n")
    );

    // Agents read descriptions but cannot write them.
    assert!(
        bbs.err(&["--token", &ta, "group", "describe", "infra", "mine now"])
            .contains("humans only")
    );
    assert!(
        bbs.err(&["--token", &ta, "group", "describe", "infra", "--clear"])
            .contains("humans only")
    );
    assert!(
        bbs.err(&[
            "--token",
            &ta,
            "group",
            "register",
            "x",
            "--description",
            "d"
        ])
        .contains("humans only")
    );

    assert_eq!(
        bbs.ok(&["group", "describe", "@plain", "Scratch space"]),
        "Updated the description of @plain.\n"
    );
    bbs.ok(&["group", "describe", "infra", "Fleet, deploys: and on-call"]);
    assert_eq!(
        bbs.ok(&["group", "list"]),
        format!(
            "@infra: {a}\n  Fleet, deploys: and on-call\n@plain: (no agents)\n  Scratch space\n"
        )
    );
    assert_eq!(
        bbs.ok(&["group", "describe", "plain", "--clear"]),
        "Cleared the description of @plain.\n"
    );
    assert!(
        bbs.ok(&["group", "list"])
            .ends_with("@plain: (no agents)\n")
    );
    // Describing is quiet metadata, not a lifecycle notice.
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");

    // Validation happens in the service and never half-applies.
    let max = "d".repeat(200);
    bbs.ok(&["group", "describe", "plain", &max]);
    for (bad, why) in [
        ("d".repeat(201), "the limit is 200"),
        ("   ".to_string(), "empty"),
        ("two\nlines".to_string(), "one line"),
        ("tab\there".to_string(), "one line"),
        ("line\u{2028}separator".to_string(), "one line"),
        ("evil\u{202E}txt.exe".to_string(), "invisible formatting"),
        ("zero\u{200B}width".to_string(), "invisible formatting"),
        ("tag\u{E0041}char".to_string(), "invisible formatting"),
    ] {
        assert!(
            bbs.err(&["group", "describe", "plain", &bad]).contains(why),
            "{bad:?}"
        );
        assert!(
            bbs.err(&["group", "register", "fresh", "--description", &bad])
                .contains(why),
            "{bad:?}"
        );
    }
    assert!(!bbs.ok(&["group", "list"]).contains("@fresh"));
    assert!(bbs.ok(&["group", "list"]).contains(&format!("  {max}\n")));
    bbs.ok(&["group", "describe", "plain", "-1 means off"]);
    assert!(bbs.ok(&["group", "list"]).contains("  -1 means off\n"));
    assert!(
        bbs.ok(&["group", "list", "--json"])
            .contains(r#""name":"plain","members":[],"description":"-1 means off","#)
    );
    bbs.ok(&["group", "describe", "plain", &max]);
    assert!(
        bbs.err(&["group", "describe", "missing", "x"])
            .contains("no such group")
    );
    assert!(
        bbs.err(&["group", "describe", "infra"])
            .contains("required arguments")
    );
    assert!(
        bbs.err(&["group", "describe", "infra", "x", "--clear"])
            .contains("cannot be used with")
    );

    // Reusing an archived name starts without the old description.
    bbs.ok(&["group", "delete", "infra"]);
    bbs.ok(&["group", "register", "infra"]);
    assert!(
        bbs.ok(&["group", "list"])
            .starts_with("@infra: (no agents)\n@plain")
    );
    let stored: Vec<Option<String>> = bbs
        .db()
        .prepare("SELECT description FROM bbs_groups WHERE name = 'infra' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored, [Some("Fleet, deploys: and on-call".into()), None]);
}

fn joining_notifies_the_group(mode: Mode) {
    let bbs = Bbs::with_mode("join", mode);
    bbs.ok(&["group", "register", "g"]);
    let (_a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "g"]);
    bbs.ok(&["--token", &tb, "group", "join", "g"]);

    let joined = format!("from {b} to @g:\nAgent {b:?} joined group @g.\n");
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&joined));
    assert!(bbs.ok(&["read"]).contains(&joined));
    assert_eq!(bbs.ok(&["--token", &tb, "read"]), "");

    // Re-joining is a no-op and announces nothing.
    assert!(
        bbs.ok(&["--token", &tb, "group", "join", "g"])
            .contains("Already in @g.")
    );
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
    assert_eq!(bbs.ok(&["read"]), "");
}

fn leaving_or_removal_notifies_the_group(mode: Mode) {
    let bbs = Bbs::with_mode("depart", mode);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, tc) = bbs.register();
    let (_d, td) = bbs.register();
    for t in [&ta, &tb, &tc] {
        bbs.ok(&["--token", t, "group", "join", "acme"]);
    }

    bbs.ok(&["--token", &tc, "group", "leave", "acme"]);
    let left = format!("from {c} to @acme:\nAgent {c:?} left group @acme.\n");
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&left));
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&left));
    assert!(bbs.ok(&["read"]).contains(&left));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert_eq!(bbs.ok(&["--token", &td, "read"]), "");

    bbs.ok(&["group", "remove", "acme", &a]);
    let removed = format!(
        "from will (human) to @acme:\nHuman \"will\" removed agent {a:?} from group @acme.\n"
    );
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&removed));
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&removed));
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&removed));
    assert!(!bbs.ok(&["read"]).contains("removed"));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert_eq!(bbs.ok(&["group", "list"]), format!("@acme: {b}\n"));
}

fn humans_grant_and_revoke_group_writers(mode: Mode) {
    let bbs = Bbs::with_mode("writers", mode);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, _tc) = bbs.register();
    for t in [&ta, &tb] {
        bbs.ok(&["--token", t, "group", "join", "acme"]);
    }
    for t in [&ta, &tb] {
        bbs.ok(&["--token", t, "read"]);
    }
    bbs.ok(&["--as", "ada", "read"]);

    // Only humans grant, and only to current members.
    assert!(
        bbs.err(&["--token", &ta, "group", "grant", "acme", &a])
            .contains("for humans only")
    );
    assert!(
        bbs.err(&["group", "grant", "acme", &c])
            .contains(&format!("{c} is not in @acme"))
    );
    assert!(
        bbs.err(&["group", "grant", "acme", "Nobody"])
            .contains("no live agent")
    );

    assert_eq!(
        bbs.ok(&["group", "grant", "@acme", &a]),
        format!("Granted write on @acme to {a}.\n")
    );
    assert_eq!(
        bbs.ok(&["group", "grant", "acme", &a]),
        format!("{a} already has write on @acme.\n")
    );
    assert_eq!(
        bbs.ok(&["group", "revoke", "acme", &b]),
        format!("{b} already lacks write on @acme.\n")
    );
    let granted = format!(
        "from will (human) to @acme:\nHuman \"will\" granted write on @acme to agent {a:?}.\n"
    );
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&granted));
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&granted));
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&granted));
    let list = bbs.ok(&["group", "list"]);
    assert!(
        list.contains(&format!("{a} [w]")) && !list.contains(&format!("{b} [w]")),
        "{list}"
    );
    let groups = json_lines(&bbs.ok(&["group", "list", "--json"]));
    assert_eq!(groups[0]["writers"], serde_json::json!([a]));
    // A refused agent learns who can broadcast.
    assert!(
        bbs.err(&["--token", &tb, "send", "--to", "@acme", "x"])
            .contains(&format!("Writers: {a} (message one of them"))
    );
    // Without write an agent still reads the group and can DM members.
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(bbs.ok(&["--token", &tb, "read"]).contains("standup"));
    bbs.ok(&["--token", &tb, "send", "--to", &a, "dm"]);
    assert!(bbs.ok(&["--token", &ta, "read"]).contains("dm"));

    // A grant outlives leaving and rejoining, and the rejoin says so.
    bbs.ok(&["--token", &ta, "group", "leave", "acme"]);
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&format!(
        "Agent {a:?} left group @acme. @acme now has no agent writers.\n"
    )));
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    assert!(
        bbs.ok(&["--as", "ada", "read"])
            .contains(&format!("Agent {a:?} joined group @acme as a writer.\n"))
    );
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "back"]);

    // Revoking needs no membership, and changes nothing else when the group
    // already had no writers among its members.
    bbs.ok(&["--token", &ta, "group", "leave", "acme"]);
    bbs.ok(&["--token", &tb, "read"]);
    assert_eq!(
        bbs.ok(&["group", "revoke", "acme", &a]),
        format!("Revoked write on @acme from {a}.\n")
    );
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&format!(
        "Human \"will\" revoked write on @acme from agent {a:?}.\n"
    )));
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "x"])
            .contains("@acme has no agent writers.")
    );

    // Retiring drops the grant: a new agent with that name lacks it.
    bbs.grant_write("acme", &a);
    bbs.ok(&["--token", &tb, "read"]);
    bbs.ok(&["--token", &ta, "bye"]);
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&format!(
        "Agent {a:?} is no longer a member of group @acme. @acme now has no agent writers.\n"
    )));
    let writers: i64 = bbs
        .db()
        .query_row("SELECT COUNT(*) FROM group_writers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(writers, 0);
    let (_, ta) = bbs.register_with(&["register", "--name", &a]);
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "x"])
            .contains("lack write access")
    );

    // Losing the last writer by revoke, removal, or retirement is announced.
    bbs.ok(&["--token", &ta, "read"]);
    bbs.grant_write("acme", &b);
    bbs.ok(&["group", "revoke", "acme", &b]);
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&format!(
        "revoked write on @acme from agent {b:?}. @acme now has no agent writers.\n"
    )));
    bbs.grant_write("acme", &b);
    bbs.ok(&["group", "remove", "acme", &b]);
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&format!(
        "removed agent {b:?} from group @acme. @acme now has no agent writers.\n"
    )));
    // Removal revokes write, so rejoining does not restore it.
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    assert!(
        bbs.err(&["--token", &tb, "send", "--to", "@acme", "x"])
            .contains("lack write access")
    );
    bbs.grant_write("acme", &b);
    bbs.ok(&["retire", &b]);
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&format!(
        "Agent {b:?} is no longer a member of group @acme. @acme now has no agent writers.\n"
    )));
}

fn humans_add_agents_to_groups(mode: Mode) {
    let bbs = Bbs::with_mode("group-add", mode);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, _tc) = bbs.register_with(&["--as", "ada", "register"]);

    assert!(
        bbs.err(&["--token", &ta, "group", "add", "acme", &a])
            .contains("for humans only")
    );
    assert!(bbs.err(&["group", "add", "acme"]).contains("required"));
    // All or nothing: one bad name adds no one.
    assert!(
        bbs.err(&["group", "add", "acme", &a, "Nobody"])
            .contains("no live agent named \"Nobody\"")
    );
    let (d, _td) = bbs.register();
    bbs.ok(&["retire", &d]);
    assert!(
        bbs.err(&["group", "add", "acme", &a, &d])
            .contains(&format!("no live agent named {d:?}"))
    );
    assert!(
        bbs.err(&["group", "add", "nope", &a])
            .contains("no such group")
    );
    assert_eq!(bbs.ok(&["group", "list"]), "@acme: (no agents)\n");

    assert_eq!(
        bbs.ok(&["group", "add", "@acme", &a, &b.to_lowercase()]),
        format!("Added {a}, {b} to @acme.\n")
    );
    let notice = |agent: &str, suffix: &str| {
        format!(
            "from will (human) to @acme:\nHuman \"will\" added agent {agent:?} to group @acme{suffix}.\n"
        )
    };
    let ada = bbs.ok(&["--as", "ada", "read"]);
    assert!(
        ada.contains(&notice(&a, "")) && ada.contains(&notice(&b, "")),
        "{ada}"
    );
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&notice(&b, "")));
    assert_eq!(
        bbs.ok(&["group", "add", "acme", &a, &b, &a]),
        format!("Already in @acme: {a}, {b}.\n")
    );
    // Membership is real: the agents share a group now.
    bbs.ok(&["--token", &ta, "send", "--to", &b, "hi"]);
    assert!(bbs.ok(&["--token", &tb, "read"]).contains("hi"));

    // Adding grants nothing, but a kept grant is announced.
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "x"])
            .contains("lack write access")
    );
    bbs.grant_write("acme", &a);
    bbs.ok(&["--token", &ta, "group", "leave", "acme"]);
    bbs.ok(&["--as", "ada", "read"]);
    bbs.ok(&["group", "add", "acme", &a]);
    assert!(
        bbs.ok(&["--as", "ada", "read"])
            .contains(&notice(&a, " as a writer"))
    );
    // Any human may add any agent, including another human's.
    assert_eq!(
        bbs.ok(&["group", "add", "acme", &c]),
        format!("Added {c} to @acme.\n")
    );
}

fn agents_lists_status_and_scopes_visibility(mode: Mode) {
    let bbs = Bbs::with_mode("agents", mode);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, _tc) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "status", "refactoring db layer"]);

    let seen_by_a = bbs.ok(&["--token", &ta, "agents"]);
    assert!(
        seen_by_a.contains(&b) && seen_by_a.contains("refactoring db layer"),
        "{seen_by_a}"
    );
    assert!(!seen_by_a.contains(&c), "{seen_by_a}");
    // Agents may message any human, so they see them all.
    assert!(
        seen_by_a
            .lines()
            .any(|l| l.split_whitespace().take(2).eq(["will", "human"])),
        "{seen_by_a}"
    );
    let seen_by_human = bbs.ok(&["agents"]);
    assert!(seen_by_human.contains(&a) && seen_by_human.contains(&c));
    assert!(seen_by_human.contains("will"));

    // OWNER follows KIND for agent and human callers; `-` when there is none.
    bbs.ok(&["human", "add", "ada"]);
    let (d, td) = bbs.register_with(&["--as", "ada", "register"]);
    bbs.ok(&["--token", &td, "group", "join", "acme"]);
    bbs.db()
        .execute(
            "DELETE FROM agent_owners WHERE agent_id = (SELECT id FROM agents WHERE name = ?1)",
            [&c],
        )
        .unwrap();
    let owner = |table: &str, name: &str| {
        let row = table
            .lines()
            .find(|line| line.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing: {table}"));
        row.split_whitespace().nth(2).unwrap().to_string()
    };
    let seen_by_a = bbs.ok(&["--token", &ta, "agents"]);
    assert_eq!(owner(&seen_by_a, "NAME"), "OWNER");
    assert_eq!(owner(&seen_by_a, &b), "will");
    assert_eq!(owner(&seen_by_a, &d), "ada");
    let seen_by_human = bbs.ok(&["agents"]);
    assert_eq!(owner(&seen_by_human, &a), "will");
    assert_eq!(owner(&seen_by_human, &c), "-");
    assert_eq!(owner(&seen_by_human, &d), "ada");
    assert_eq!(owner(&seen_by_human, "will"), "-");
}

fn hook(bbs: &Bbs, event: serde_json::Value) -> Option<serde_json::Value> {
    let out = bbs.run_with_stdin(&["hook", "claude-code"], &event.to_string());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    (!stdout.trim().is_empty()).then(|| serde_json::from_str(&stdout).unwrap())
}

fn active_at_writes_are_debounced(mode: Mode) {
    let bbs = Bbs::with_mode("debounce", mode);
    let (_, ta) = bbs.register();
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "whoami"]);

    // Freshly active callers do not write, so other connections see no change.
    let db = bbs.db();
    let version = || -> i64 {
        db.pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap()
    };
    let before = version();
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "agents"]);
    assert_eq!(version(), before);

    // Stale timestamps are still refreshed.
    db.execute_batch(
        "UPDATE agents SET active_at = '2000-01-01T00:00:00.000000Z';
         UPDATE humans SET active_at = '2000-01-01T00:00:00.000000Z';",
    )
    .unwrap();
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "whoami"]);
    let stale: i64 = db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM agents WHERE active_at < '2001')
                  + (SELECT COUNT(*) FROM humans WHERE active_at < '2001')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stale, 0);
}

fn claude_code_hooks_bind_and_restore_identity(mode: Mode) {
    let bbs = Bbs::with_mode("hook", mode);
    let register_out = bbs.ok(&["register"]);
    let (a, ta) = (
        register_out.split('"').nth(1).unwrap().to_string(),
        register_out
            .split("--token ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string(),
    );
    bbs.ok(&["send", "--to", &a, "before binding"]);

    let post_tool = |command: &str, stdout: &str| {
        serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_response": { "stdout": stdout, "stderr": "", "interrupted": false, "isImage": false },
        })
    };
    let event = |name: &str| serde_json::json!({ "session_id": "s1", "hook_event_name": name });
    let compact = serde_json::json!({ "session_id": "s1", "hook_event_name": "SessionStart", "source": "compact" });

    // Unbound sessions are a silent no-op.
    assert!(hook(&bbs, compact.clone()).is_none());
    assert!(hook(&bbs, serde_json::json!({"garbage": true})).is_none());

    // Seeing `bbs register` output binds the session without taking mail.
    assert!(hook(&bbs, post_tool("bbs register --note x", &register_out)).is_none());
    bbs.ok(&["send", "--to", &a, "after binding"]);
    assert!(hook(&bbs, post_tool("ls", "")).is_none());
    for name in ["UserPromptSubmit", "Stop", "SessionStart"] {
        assert!(hook(&bbs, event(name)).is_none(), "{name}");
    }

    // After compaction the identity is re-injected.
    let out = hook(&bbs, compact).unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(&ta)
    );

    // Mail is left for the listener.
    let mail = bbs.ok(&["--token", &ta, "read"]);
    assert!(
        mail.contains("before binding") && mail.contains("after binding"),
        "{mail}"
    );

    // Re-registering in the same session retires the old identity.
    let second = bbs.ok(&["register"]);
    hook(&bbs, post_tool("bbs register", &second));
    assert!(bbs.err(&["--token", &ta, "read"]).contains("retired"));
}

fn codex_hooks_bind_from_string_tool_response(mode: Mode) {
    let bbs = Bbs::with_mode("codex", mode);
    let register_out = bbs.ok(&["register"]);
    let token = register_out
        .split("--token ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let codex = |event: serde_json::Value| {
        let out = bbs.run_with_stdin(&["hook", "codex"], &event.to_string());
        assert!(out.status.success());
        let stdout = String::from_utf8(out.stdout).unwrap();
        (!stdout.trim().is_empty())
            .then(|| serde_json::from_str::<serde_json::Value>(&stdout).unwrap())
    };
    let bind = serde_json::json!({
        "session_id": "c1",
        "turn_id": "t1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "cd /repo && bbs register --note 'codex'" },
        "tool_response": register_out,
    });
    assert!(codex(bind).is_none());
    bbs.ok(&[
        "send",
        "--to",
        register_out.split('"').nth(1).unwrap(),
        "hi codex",
    ]);

    // Bindings are per harness: the same session id under claude-code is unbound.
    let compact = serde_json::json!({ "session_id": "c1", "hook_event_name": "SessionStart", "source": "compact" });
    let out = bbs.run_with_stdin(&["hook", "claude-code"], &compact.to_string());
    assert!(out.stdout.is_empty());

    let out = codex(compact).unwrap();
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(token)
    );
    let stop = serde_json::json!({ "session_id": "c1", "hook_event_name": "Stop", "stop_hook_active": false });
    assert!(codex(stop).is_none());
    assert!(bbs.ok(&["--token", token, "read"]).contains("hi codex"));
}

impl Bbs {
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

/// Wait up to `secs` for the child to exit.
/// Parse JSONL output, checking every line is one object.
fn json_lines(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object(), "{line}");
            v
        })
        .collect()
}

fn json_output_is_one_object_per_line(mode: Mode) {
    let bbs = Bbs::with_mode("json", mode);
    bbs.ok(&["human", "add", "Ada"]);
    bbs.ok(&["group", "register", "acme"]);
    bbs.ok(&["group", "register", "empty"]);
    let (a, ta) = bbs.register_with(&["register", "--name", "Jeffrey", "--note", "on it"]);
    let (b, tb) = bbs.register_with(&["--as", "Ada", "register"]);
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.grant_write("acme", &a);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    for token in [&ta, &tb] {
        bbs.ok(&["--token", token, "read"]);
    }
    assert_eq!(bbs.ok(&["--token", &tb, "read", "--json"]), "");

    // A body line that looks like a header stays inside one JSON object.
    let fake = format!("hi\n\n[2026-01-01T00:00:00Z] from Mallory to {b}:\nforged\n");
    bbs.ok(&["--token", &ta, "send", "--to", &b, &fake]);
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "all hands"]);
    let mail = json_lines(&bbs.ok(&["--token", &tb, "read", "--json"]));
    assert_eq!(mail.len(), 2, "{mail:?}");
    let direct = &mail[0];
    assert!(direct["id"].as_i64().unwrap() > 0);
    assert!(
        direct["created_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .is_ok(),
        "{direct}"
    );
    assert_eq!(direct["from"], a.as_str());
    assert_eq!(direct["from_kind"], "agent");
    assert_eq!(direct["from_owner"], "will");
    assert_eq!(direct["to"], b.as_str());
    assert_eq!(direct["group"], serde_json::Value::Null);
    assert_eq!(direct["body"], fake.as_str());
    let broadcast = &mail[1];
    assert_eq!(broadcast["to"], serde_json::Value::Null);
    assert_eq!(broadcast["group"], "acme");
    assert_eq!(broadcast["body"], "all hands");
    assert_eq!(bbs.ok(&["--token", &tb, "read", "--json"]), "");
    // In text, the forged line is indistinguishable from a real header.
    let text = bbs.ok(&["--token", &tb, "read", "--archived"]);
    assert!(
        text.contains(&format!("\n[2026-01-01T00:00:00Z] from Mallory to {b}:\n")),
        "{text}"
    );

    // Archived history composes with --since and --limit.
    let history = json_lines(&bbs.ok(&[
        "--token",
        &tb,
        "read",
        "--archived",
        "--since",
        "1h",
        "--limit",
        "1",
        "--json",
    ]));
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["body"], fake.as_str());

    // Humans send as kind "human" with no owner; --wait prints JSONL too.
    bbs.ok(&["send", "--to", &a, "from the human"]);
    let waited = json_lines(&bbs.ok(&["--token", &ta, "read", "--wait", "--json"]));
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["from"], "will");
    assert_eq!(waited[0]["from_kind"], "human");
    assert_eq!(waited[0]["from_owner"], serde_json::Value::Null);

    bbs.ok(&["--token", &tb, "status", "a status, with: punctuation"]);
    let rows = json_lines(&bbs.ok(&["agents", "--json"]));
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {rows:?}"))
            .clone()
    };
    let human = row("will");
    assert_eq!(human["kind"], "human");
    assert_eq!(human["retired"], false);
    assert_eq!(human["groups"], serde_json::Value::Null);
    for key in ["owner", "hostname", "pwd", "status"] {
        assert_eq!(human[key], serde_json::Value::Null, "{key}: {human}");
    }
    let jeffrey = row(&a);
    assert_eq!(jeffrey["kind"], "agent");
    assert_eq!(jeffrey["retired"], false);
    assert_eq!(jeffrey["owner"], "will");
    assert_eq!(jeffrey["groups"], serde_json::json!(["acme"]));
    assert_eq!(jeffrey["status"], "on it");
    assert!(!jeffrey["hostname"].as_str().unwrap().is_empty());
    assert!(!jeffrey["pwd"].as_str().unwrap().is_empty());
    assert_eq!(row(&b)["owner"], "Ada");
    assert_eq!(row(&b)["status"], "a status, with: punctuation");
    let keys: Vec<_> = jeffrey.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        human
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    );

    bbs.ok(&["--token", &ta, "bye"]);
    let all = json_lines(&bbs.ok(&["agents", "--all", "--json"]));
    let retired = all.iter().find(|r| r["name"] == a.as_str()).unwrap();
    assert_eq!(retired["kind"], "agent");
    assert_eq!(retired["retired"], true);

    let groups = json_lines(&bbs.ok(&["group", "list", "--json"]));
    assert_eq!(
        groups,
        vec![
            serde_json::json!({"name": "acme", "members": [b], "description": null, "writers": []}),
            serde_json::json!({"name": "empty", "members": [], "description": null, "writers": []}),
        ]
    );
}

fn wait_exit(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

fn read_wait_blocks_until_mail_arrives(mode: Mode) {
    let bbs = Bbs::with_mode("wait", mode);
    let (a, ta) = bbs.register();
    let (b, _tb) = bbs.register();
    assert!(
        bbs.err(&["read", "--wait", "--archived"])
            .contains("cannot be used with")
    );

    // Already-unread mail returns immediately.
    bbs.ok(&["send", "--to", &a, "early"]);
    assert!(
        bbs.ok(&["--token", &ta, "read", "--wait"])
            .contains("early")
    );

    let mut child = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none(), "returned without mail");

    // Mail for someone else does not wake it.
    bbs.ok(&["send", "--to", &b, "not for a"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "woke for another inbox"
    );

    let sent = std::time::Instant::now();
    bbs.ok(&["send", "--to", &a, "wake up"]);
    let status = wait_exit(&mut child, 5).expect("read --wait did not wake");
    assert!(status.success());
    assert!(sent.elapsed() < std::time::Duration::from_millis(2500));
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("wake up") && !stdout.contains("not for a"),
        "{stdout}"
    );
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
}

fn only_one_read_wait_per_inbox(mode: Mode) {
    let bbs = Bbs::with_mode("wait-lock", mode);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let mut first = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(500));

    // A second waiter fails fast; other inboxes are unaffected. Try it before
    // sending, since a waiter that gets mail exits and frees the inbox at once.
    let err = bbs.err(&["--token", &ta, "read", "--wait"]);
    assert!(err.contains("already running"), "{err}");
    let mut other = bbs.spawn(&["--token", &tb, "read", "--wait"]);
    bbs.ok(&["send", "--to", &a, "for the first waiter"]);

    let status = wait_exit(&mut first, 5).expect("first waiter did not wake");
    assert!(status.success());
    let out = String::from_utf8(first.wait_with_output().unwrap().stdout).unwrap();
    assert!(out.contains("for the first waiter"), "{out}");
    assert!(
        other.try_wait().unwrap().is_none(),
        "other inbox's waiter exited"
    );
    other.kill().unwrap();
    other.wait().unwrap();

    // The lock is released on exit, even when the waiter is killed.
    bbs.ok(&["send", "--to", &a, "again"]);
    assert!(
        bbs.ok(&["--token", &ta, "read", "--wait"])
            .contains("again")
    );
    bbs.ok(&["send", "--to", &b, "after kill"]);
    assert!(
        bbs.ok(&["--token", &tb, "read", "--wait"])
            .contains("after kill")
    );
}

fn read_wait_fails_if_retired_while_waiting(mode: Mode) {
    let bbs = Bbs::with_mode("wait-retired", mode);
    let (_a, ta) = bbs.register();
    let mut child = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    bbs.ok(&["--token", &ta, "bye"]);
    let status = wait_exit(&mut child, 5).expect("waiter did not notice retirement");
    assert!(!status.success());
    let err = String::from_utf8(child.wait_with_output().unwrap().stderr).unwrap();
    assert!(err.contains("retired"), "{err}");
}

#[test]
fn old_schema_is_migrated_on_open_even_concurrently() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-migrate", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let state = home.join(".local/state/bbs");
    std::fs::create_dir_all(&state).unwrap();
    let db = rusqlite::Connection::open(state.join("bbs.db")).unwrap();
    db.execute_batch(include_str!("../crates/bbs-core/migrations/0001_init.sql"))
        .unwrap();
    db.execute_batch(
        "PRAGMA user_version = 1;
         INSERT INTO humans (name, created_at, active_at) VALUES ('will', 'x', 'x');",
    )
    .unwrap();
    drop(db);

    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_bbs"))
                .arg("agents")
                .env("HOME", &home)
                .env("USER", "will")
                .env_remove("BBS_SERVER")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let db = rusqlite::Connection::open(state.join("bbs.db")).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version >= 4, "schema still at v{version}");
    std::fs::remove_dir_all(&home).unwrap();
}

fn view_is_for_humans_only(mode: Mode) {
    let bbs = Bbs::with_mode("view", mode);
    let (_a, ta) = bbs.register();
    for args in [&["view", "--no-follow"][..], &["view", "--json"]] {
        let mut args = args.to_vec();
        args.splice(0..0, ["--token", ta.as_str()]);
        assert!(bbs.err(&args).contains("humans only"));
    }
    // The TUI refuses to start without a terminal instead of garbling a pipe.
    assert!(bbs.err(&["view"]).contains("needs a terminal"));
}

/// `bbs view --json --no-follow` with `args`, parsed.
fn view_json(bbs: &Bbs, args: &[&str]) -> Vec<serde_json::Value> {
    let mut all = vec!["view", "--json", "--no-follow"];
    all.extend_from_slice(args);
    bbs.ok(&all)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn bodies(rows: &[serde_json::Value]) -> Vec<&str> {
    rows.iter().map(|r| r["body"].as_str().unwrap()).collect()
}

fn blocker_tools_and_cli_keep_versions_retries_and_answer_delivery(mode: Mode) {
    use serde_json::{Value, json};
    let bbs = Bbs::with_mode("blockers-tools", mode);
    let (agent, token) = bbs.register();
    bbs.ok(&["group", "register", "crew"]);
    bbs.ok(&["--token", &token, "group", "join", "crew"]);
    bbs.ok(&["--token", &token, "read"]);
    bbs.ok(&["read"]);
    let catalog = bbs.run_with_env(
        &["headless", "mcp"],
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n",
        &[("BBS_HEADLESS_TOKEN", &token)],
    );
    assert!(catalog.status.success());
    let catalog: Value = serde_json::from_slice(&catalog.stdout).unwrap();
    let tools = catalog["result"]["tools"].as_array().unwrap();
    for name in [
        "bbs_blockers",
        "bbs_blocker_get",
        "bbs_blocker_put",
        "bbs_blocker_cancel",
    ] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert!(!tools.iter().any(|t| t["name"] == "bbs_blocker_answer"));
    let call = |name: &str, args: Value| -> Value {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":args}});
        let out = bbs.run_with_env(
            &["headless", "mcp"],
            &format!("{request}\n"),
            &[("BBS_HEADLESS_TOKEN", &token)],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let success = |name: &str, args: Value| -> Value {
        let response = call(name, args);
        assert_ne!(response["result"]["isError"], true, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let args = json!({"group":"crew","body":"  Need approval\nOption A?","request_key":"first"});
    let first = success("bbs_blocker_put", args.clone());
    let id = first["blocker"]["id"].as_i64().unwrap();
    assert_eq!(success("bbs_blocker_put", args)["replayed"], true);
    assert_eq!(
        bbs.ok(&["read"]),
        "",
        "creating a blocker sends no owner mail"
    );
    assert_eq!(bbs.ok(&["--token", &token, "read"]), "");
    let listing = success("bbs_blockers", json!({"agent":agent,"limit":1}));
    assert_eq!(listing["active_count"], 1);
    assert_eq!(listing["blockers"][0]["body"], "  Need approval\nOption A?");
    let next = success(
        "bbs_blocker_put",
        json!({"group":"crew","body":"Need approval for B instead","replaces":id,"request_key":"second"}),
    );
    let next_id = next["blocker"]["id"].as_i64().unwrap();
    assert_ne!(next_id, id);
    assert_eq!(next["blocker"]["root_id"], id);
    assert_eq!(
        call(
            "bbs_blocker_put",
            json!({"group":"crew","body":"changed under first key","request_key":"first"})
        )["result"]["isError"],
        true
    );
    assert_eq!(
        call(
            "bbs_blocker_put",
            json!({"group":"crew","body":"missing key"})
        )["result"]["isError"],
        true
    );
    assert_eq!(
        call("bbs_blockers", json!({"limit":"1"}))["result"]["isError"],
        true
    );
    assert_eq!(
        call("bbs_blocker_get", json!({"id":id,"unknown":1}))["result"]["isError"],
        true
    );
    let next_s = next_id.to_string();
    let answer = bbs.ok(&[
        "blocker",
        "answer",
        &next_s,
        "Proceed with B",
        "--request-key",
        "answer",
        "--json",
    ]);
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["blocker"]["status"], "answered");
    let replay: Value = serde_json::from_str(&bbs.ok(&[
        "blocker",
        "answer",
        &next_s,
        "Proceed with B",
        "--request-key",
        "answer",
        "--json",
    ]))
    .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["message_id"], answer["message_id"]);
    let mail = bbs.ok(&["--token", &token, "read"]);
    assert!(mail.contains("Proceed with B"), "{mail}");
    assert_eq!(
        bbs.ok(&["--token", &token, "read"]),
        "",
        "no duplicate answer delivery"
    );
    let history = success("bbs_blocker_get", json!({"id":id,"limit":1}));
    assert_eq!(history["blocker"]["body"], "  Need approval\nOption A?");
    assert_eq!(history["blocker"]["status"], "superseded");
    assert_eq!(history["current"]["id"], next_id);
    assert_eq!(history["history"][0]["outcome"]["answer"], "Proceed with B");
    assert_eq!(history["has_more"], true);
    let older = success(
        "bbs_blocker_get",
        json!({"id":id,"limit":1,"before_id":history["next_before_id"]}),
    );
    assert_eq!(older["history"][0]["id"], id);
    assert_eq!(older["has_more"], false);
    let put_out = bbs.run_with_stdin(
        &[
            "--token",
            &token,
            "blocker",
            "put",
            "--group",
            "crew",
            "--replaces",
            &next_s,
            "--request-key",
            "third",
            "--json",
        ],
        "A follow-up after the answer\n",
    );
    assert!(
        put_out.status.success(),
        "{}",
        String::from_utf8_lossy(&put_out.stderr)
    );
    let third: Value = serde_json::from_slice(&put_out.stdout).unwrap();
    assert_eq!(third["blocker"]["body"], "A follow-up after the answer\n");
    let third_id = third["blocker"]["id"].as_i64().unwrap();
    let cancelled = success(
        "bbs_blocker_cancel",
        json!({"id":third_id,"request_key":"cancel"}),
    );
    assert_eq!(cancelled["blocker"]["status"], "cancelled");
    assert_eq!(
        success(
            "bbs_blocker_cancel",
            json!({"id":third_id,"request_key":"cancel"})
        )["replayed"],
        true
    );
    assert_eq!(bbs.ok(&["read"]), "", "cancellation sends no owner mail");
    let list: Value = serde_json::from_str(&bbs.ok(&[
        "blocker",
        "list",
        "--group",
        "crew",
        "--include-closed",
        "--json",
    ]))
    .unwrap();
    assert_eq!(list["active_count"], 0);
    assert_eq!(list["blockers"][0]["id"], third_id);
    let mine: Value =
        serde_json::from_str(&bbs.ok(&["blocker", "list", "--mine", "--include-closed", "--json"]))
            .unwrap();
    assert_eq!(mine["blockers"][0]["id"], third_id);
    assert_eq!(mine["active_count"], 0);
    assert!(
        bbs.err(&["blocker", "list", "--mine", "--group", "crew"])
            .contains("cannot be used")
    );
    assert!(
        bbs.err(&["--token", &token, "blocker", "list", "--mine"])
            .contains("human")
    );
    assert!(
        bbs.ok(&["blocker", "get", &id.to_string()])
            .contains("History (newest first)")
    );
    assert!(
        bbs.err(&["blocker", "list", "--group", "crew", "--agent", &agent])
            .contains("cannot be used")
    );
}

fn headless_search_reads_the_corpus_and_pages_without_archiving(mode: Mode) {
    let bbs = Bbs::with_mode("search-tools", mode);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    bbs.ok(&["send", "--to", &b, "older Équipe [a.*]_%"]);
    bbs.ok(&["send", "--to", &b, "newer Équipe [a.*]_%"]);
    let search = |args: serde_json::Value| -> serde_json::Value {
        let input = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"bbs_search","arguments":args}})
        .to_string();
        let out = bbs.run_with_env(
            &["headless", "mcp"],
            &format!("{input}\n"),
            &[("BBS_HEADLESS_TOKEN", &ta)],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_ne!(response["result"]["isError"], true, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let page = search(serde_json::json!({"query":"équipe [a.*]_%","limit":1,"since":"7d"}));
    assert_eq!(page["hits"].as_array().unwrap().len(), 1);
    assert_eq!(page["hits"][0]["body"], "newer Équipe [a.*]_%");
    assert_eq!(page["hits"][0]["to"]["name"], b);
    assert_eq!(page["hits"][0]["from"]["name"], "will");
    assert_eq!(page["has_more"], true);
    assert!(page["since"].as_str().unwrap().contains('T'));
    assert_ne!(a, b, "caller has no inbox copy of the search hits");
    bbs.ok(&["send", "--to", &b, "newest Équipe [a.*]_%"]);
    let older = search(
        serde_json::json!({"query":"équipe [a.*]_%","limit":1,"before_id":page["next_before_id"],"since":page["since"]}),
    );
    assert_eq!(older["hits"][0]["body"], "older Équipe [a.*]_%");
    assert_eq!(older["has_more"], false);
    assert!(older["next_before_id"].is_null());
    let exact = search(serde_json::json!({"query":"équipe","case_sensitive":true}));
    assert_eq!(exact["hits"], serde_json::json!([]));
    assert!(
        bbs.ok(&["--token", &tb, "read"]).contains("older Équipe"),
        "still unread after another agent searched"
    );
    let list = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
    let out = bbs.run_with_env(&["headless", "mcp"], list, &[("BBS_HEADLESS_TOKEN", &ta)]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "bbs_search")
    );
}

fn headless_agents_manage_timers_with_tools(mode: Mode) {
    let bbs = Bbs::with_mode("timer-tools", mode);
    let (a, ta) = bbs.register();
    let call = |id: u32, name: &str, args: serde_json::Value| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": args },
        })
        .to_string()
    };
    let input = [
        call(
            1,
            "bbs_timer_add",
            serde_json::json!({ "agent": a, "when": "every 1h", "text": "stretch" }),
        ),
        call(2, "bbs_timer_list", serde_json::json!({})),
        call(
            3,
            "bbs_timer_add",
            serde_json::json!({ "agent": a, "when": "every 1m", "text": "x" }),
        ),
        call(4, "bbs_timer_rm", serde_json::json!({ "id": 1 })),
        call(5, "bbs_timer_list", serde_json::json!({})),
    ]
    .join("\n");
    let out = bbs.run_with_env(
        &["headless", "mcp"],
        &format!("{input}\n"),
        &[("BBS_HEADLESS_TOKEN", &ta)],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let replies: Vec<(String, bool)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let reply: serde_json::Value = serde_json::from_str(line).unwrap();
            let result = &reply["result"];
            let text = result["content"][0]["text"].as_str().unwrap().to_string();
            (text, result["isError"] == true)
        })
        .collect();
    assert_eq!(replies.len(), 5, "{replies:?}");
    assert!(
        replies[0]
            .0
            .starts_with(&format!("Timer 1 for {a}: every 1h, next at ")),
        "{replies:?}"
    );
    assert!(
        replies[1].0.contains(&format!("from {a}: stretch")),
        "{replies:?}"
    );
    assert!(
        replies[2].1 && replies[2].0.contains("shortest interval is 5m"),
        "{replies:?}"
    );
    assert_eq!(replies[3], (format!("Removed timer 1 for {a}."), false));
    assert_eq!(replies[4], ("No timers.".to_string(), false));
}

fn timers_are_set_listed_fired_and_removed(mode: Mode) {
    let bbs = Bbs::with_mode("timers", mode);
    bbs.ok(&["group", "register", "crew"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (_c, tc) = bbs.register();
    for token in [&ta, &tb] {
        bbs.ok(&["--token", token, "group", "join", "crew"]);
    }

    let set = bbs.ok(&[
        "--token",
        &ta,
        "timer",
        "add",
        &b,
        "every 1h",
        "check the build",
    ]);
    assert!(
        set.starts_with(&format!("Timer 1 for {b}: every 1h, next at ")),
        "{set}"
    );
    bbs.ok(&["--token", &ta, "timer", "add", &a, "0 9 * * 1-5", "standup"]);
    // Agents see their own timers by default; humans see everyone's.
    let mine = bbs.ok(&["--token", &tb, "timer", "list"]);
    assert!(
        mine.contains(&format!("1  {b}  every 1h, next at ")),
        "{mine}"
    );
    assert!(
        mine.contains(&format!("from {a}: check the build")),
        "{mine}"
    );
    assert_eq!(bbs.ok(&["timer", "list"]).lines().count(), 2);
    for (args, why) in [
        (
            vec!["--token", &tc, "timer", "add", &b, "in 1h", "x"],
            "is not in any of your groups",
        ),
        (
            vec!["--token", &ta, "timer", "add", &b, "soonish", "x"],
            "invalid schedule",
        ),
        (
            vec!["--token", &ta, "timer", "add", &b, "every 1m", "x"],
            "shortest interval is 5m",
        ),
        (
            vec!["--token", &tc, "timer", "rm", "1"],
            "is not in any of your groups",
        ),
    ] {
        let err = bbs.err(&args);
        assert!(err.contains(why), "{args:?}: {err}");
    }

    // Make it due now, then wake the scheduler with any write; read --wait
    // returns once it has fired.
    bbs.db()
        .execute(
            "UPDATE timers SET next_run = '2026-01-01T00:00:00.000000Z' WHERE id = 1",
            [],
        )
        .unwrap();
    bbs.ok(&["--token", &tb, "status", "waiting for a timer"]);
    let mail = bbs.ok(&["--token", &tb, "read", "--wait"]);
    assert!(
        mail.contains(&format!("from {a} to {b} (timer 1):\ncheck the build")),
        "{mail}"
    );
    // Rescheduled from now, an hour out.
    let next: String = bbs
        .db()
        .query_row("SELECT next_run FROM timers WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert!(next.as_str() > "2026-01-01T01:00:00", "{next}");

    assert_eq!(
        bbs.ok(&["--token", &tb, "timer", "rm", "1"]),
        format!("Removed timer 1 for {b}.\n")
    );
    assert!(bbs.ok(&["--token", &tb, "timer", "list"]).is_empty());
}

fn view_with_shows_what_a_name_sent_and_was_sent_directly(mode: Mode) {
    let bbs = Bbs::with_mode("view-with", mode);
    bbs.ok(&["group", "register", "crew"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (_c, tc) = bbs.register();
    for token in [&ta, &tb, &tc] {
        bbs.ok(&["--token", token, "group", "join", "crew"]);
    }
    bbs.grant_write("crew", &a);
    bbs.grant_write("crew", &b);
    bbs.ok(&["--token", &ta, "send", "--to", &b, "A to B"]);
    bbs.ok(&["--token", &tb, "send", "--to", &a, "B to A"]);
    bbs.ok(&["--token", &tc, "send", "--to", &b, "C to B"]);
    bbs.ok(&["--token", &ta, "send", "--to", "@crew", "A to crew"]);
    bbs.ok(&["--token", &tb, "send", "--to", "@crew", "B to crew"]);
    bbs.ok(&["send", "--to", &a, "will to A"]);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["--as", "ada", "send", "--to", "will", "ada to will"]);

    // What A sent, anywhere (its join notice too), and direct messages to A;
    // not broadcasts A received.
    let with_a = view_json(&bbs, &["--with", &a.to_lowercase()]);
    let joined = format!("Agent \"{a}\" joined group @crew.");
    assert_eq!(
        bodies(&with_a),
        [
            joined.as_str(),
            "A to B",
            "B to A",
            "A to crew",
            "will to A"
        ]
    );
    // Other filters still apply.
    let with_a_crew = view_json(&bbs, &["--with", &a, "--group", "crew"]);
    assert_eq!(bodies(&with_a_crew), [joined.as_str(), "A to crew"]);
    let limited = view_json(&bbs, &["--with", &a, "--limit", "2"]);
    assert_eq!(bodies(&limited), ["A to crew", "will to A"]);
    // A human's name: what they sent (the write grants too) and direct
    // messages to them.
    let with_will = view_json(&bbs, &["--with", "will"]);
    assert_eq!(
        bodies(&with_will),
        [
            format!("Human \"will\" granted write on @crew to agent \"{a}\"."),
            format!("Human \"will\" granted write on @crew to agent \"{b}\"."),
            "will to A".to_string(),
            "ada to will".to_string(),
        ]
    );
    assert!(
        bbs.err(&["view", "--json", "--no-follow", "--with", "nobody"])
            .contains("no agent or human named \"nobody\"")
    );
}

fn view_filters_by_group_sender_owner_and_scope(mode: Mode) {
    let bbs = Bbs::with_mode("view-filters", mode);
    bbs.ok(&["human", "add", "ada"]);
    for g in ["acme", "beta", "gamma"] {
        bbs.ok(&["group", "register", g]);
    }
    // will owns A (acme) and B (beta); ada owns C (acme) and D (beta, gamma).
    let (a, ta) = bbs.register();
    let (_b, tb) = bbs.register();
    let (c, tc) = bbs.register_with(&["--as", "ada", "register"]);
    let (d, td) = bbs.register_with(&["--as", "ada", "register"]);
    for (token, group) in [(&ta, "acme"), (&tb, "beta"), (&tc, "acme")] {
        bbs.ok(&["--token", token, "group", "join", group]);
    }
    bbs.ok(&["--token", &td, "group", "join", "beta"]);
    bbs.ok(&["--token", &td, "group", "join", "gamma"]);
    bbs.grant_write("acme", &a);
    bbs.grant_write("beta", &d);
    bbs.grant_write("gamma", &d);

    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "acme from A"]);
    bbs.ok(&["--token", &td, "send", "--to", "@beta", "beta from D"]);
    bbs.ok(&["--token", &tc, "send", "--to", &a, "dm C to A"]);
    bbs.ok(&["--as", "ada", "send", "--to", &d, "ada to D"]);
    bbs.ok(&["send", "--to", "ada", "will to ada"]);
    bbs.ok(&["--as", "ada", "send", "--to", "will", "ada to will"]);
    bbs.ok(&["--token", &td, "send", "--to", "@gamma", "gamma before B"]);
    bbs.ok(&["--token", &tb, "group", "join", "gamma"]);
    bbs.ok(&["--token", &td, "send", "--to", "@gamma", "gamma after B"]);

    let all = view_json(&bbs, &[]);
    let ids: Vec<i64> = all.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "oldest first: {ids:?}");
    let acme = &all[bodies(&all)
        .iter()
        .position(|b| *b == "acme from A")
        .unwrap()];
    assert_eq!(
        acme,
        &serde_json::json!({
            "id": acme["id"], "created_at": acme["created_at"], "from": a,
            "from_kind": "agent", "from_owner": "will", "to": null, "to_owner": null,
            "group": "acme", "body": "acme from A", "read_at": null,
        })
    );
    let dm = &all[bodies(&all)
        .iter()
        .position(|b| *b == "ada to will")
        .unwrap()];
    assert_eq!(
        (&dm["from_kind"], &dm["from_owner"], &dm["to"], &dm["group"]),
        (
            &serde_json::json!("human"),
            &serde_json::Value::Null,
            &serde_json::json!("will"),
            &serde_json::Value::Null
        )
    );
    assert_eq!(dm["to_owner"], serde_json::Value::Null);
    // Names and owners stay separate fields in JSON.
    let agent_dm = &all[bodies(&all).iter().position(|b| *b == "dm C to A").unwrap()];
    assert_eq!(
        (
            &agent_dm["from"],
            &agent_dm["from_owner"],
            &agent_dm["to"],
            &agent_dm["to_owner"]
        ),
        (
            &serde_json::json!(c),
            &serde_json::json!("ada"),
            &serde_json::json!(a),
            &serde_json::json!("will")
        )
    );

    let group = view_json(&bbs, &["--group", "@acme"]);
    assert!(group.iter().all(|r| r["group"] == "acme"), "{group:?}");
    assert!(bodies(&group).contains(&"acme from A"));

    // A reused group name covers every incarnation, archived or active.
    bbs.ok(&["group", "register", "temp"]);
    bbs.ok(&["send", "--to", "@temp", "first temp"]);
    bbs.ok(&["group", "delete", "temp"]);
    bbs.ok(&["group", "register", "temp"]);
    bbs.ok(&["send", "--to", "@temp", "second temp"]);
    let temp = view_json(&bbs, &["--group", "temp", "--from", "will"]);
    assert_eq!(
        bodies(&temp),
        [
            "first temp",
            "Human \"will\" archived group @temp.",
            "second temp"
        ]
    );
    let last = view_json(&bbs, &["--group", "temp", "--limit", "1"]);
    assert_eq!(bodies(&last), ["second temp"]);

    let from = view_json(&bbs, &["--from", &a.to_lowercase()]);
    assert!(from.iter().all(|r| r["from"] == a.as_str()), "{from:?}");
    assert!(bodies(&from).contains(&"acme from A"));

    let ada = view_json(&bbs, &["--from", "@ada"]);
    let senders: std::collections::HashSet<&str> =
        ada.iter().map(|r| r["from"].as_str().unwrap()).collect();
    assert_eq!(
        senders,
        ["ada", c.as_str(), d.as_str()].into_iter().collect(),
        "{ada:?}"
    );

    // will's scope: own and own agents' mail, DMs to will or will's agents, and
    // group mail delivered to will's agents (membership at send time).
    let mine = bodies(&view_json(&bbs, &["--mine"]))
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    for body in [
        "acme from A",
        "beta from D",
        "dm C to A",
        "will to ada",
        "ada to will",
        "gamma after B",
    ] {
        assert!(mine.iter().any(|m| m == body), "{body} missing: {mine:?}");
    }
    for body in ["ada to D", "gamma before B"] {
        assert!(!mine.iter().any(|m| m == body), "{body} leaked: {mine:?}");
    }
    // Filters compose with each other and with --limit.
    let mine_gamma = view_json(&bbs, &["--mine", "--group", "gamma"]);
    assert!(bodies(&mine_gamma).contains(&"gamma after B"));
    assert!(!bodies(&mine_gamma).contains(&"gamma before B"));
    let last = view_json(&bbs, &["--from", "@ada", "--limit", "2"]);
    assert_eq!(bodies(&last), ["gamma before B", "gamma after B"]);
    // ada's scope is computed for ada.
    let ada_mine = bodies(&view_json(&bbs, &["--as", "ada", "--mine"]))
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    for body in ["gamma before B", "acme from A", "ada to D", "will to ada"] {
        assert!(ada_mine.iter().any(|m| m == body), "{body}: {ada_mine:?}");
    }

    // Plain text names owner, target, and read state.
    let text = bbs.ok(&["view", "--no-follow", "--from", "@ada", "--limit", "4"]);
    assert!(
        text.contains("from ada (human) to will (unread):\nada to will"),
        "{text}"
    );
    assert!(
        text.contains(&format!("from {d}@ada to @gamma:\ngamma after B")),
        "{text}"
    );
    // The feed qualifies every agent, including direct recipients.
    assert!(
        text.contains(&format!("from ada (human) to {d}@ada (unread):\nada to D")),
        "{text}"
    );
    let text = bbs.ok(&["view", "--no-follow", "--from", c.as_str()]);
    assert!(
        text.contains(&format!("from {c}@ada to {a}@will (unread):\ndm C to A")),
        "{text}"
    );
    bbs.ok(&["read"]);
    let text = bbs.ok(&["view", "--no-follow", "--from", "ada", "--limit", "1"]);
    assert!(text.contains("to will (read 20"), "{text}");

    // Unknown names fail loudly rather than matching nothing.
    assert!(
        bbs.err(&["view", "--json", "--group", "nope"])
            .contains("no such group @nope")
    );
    assert!(
        bbs.err(&["view", "--json", "--from", "Nobody"])
            .contains("no agent or human named \"Nobody\"")
    );
    assert!(
        bbs.err(&["view", "--json", "--from", "@nobody"])
            .contains("unknown human \"nobody\"")
    );
    assert!(
        bbs.err(&["view", "--json", "--limit", "0"])
            .contains("1..=1000")
    );

    // Inbox reads keep their own rule and carry no recipient owner.
    let inbox: serde_json::Value = serde_json::from_str(
        bbs.ok(&["--token", &ta, "read", "--json"])
            .lines()
            .find(|l| l.contains("dm C to A"))
            .unwrap(),
    )
    .unwrap();
    assert!(inbox.get("to_owner").is_none(), "{inbox}");
    let text = bbs.ok(&["--token", &ta, "read", "--archived"]);
    assert!(text.contains(&format!("from {c}@ada to {a}:")), "{text}");
}

fn view_since_and_until_bound_history(mode: Mode) {
    let bbs = Bbs::with_mode("view-time", mode);
    let (a, _ta) = bbs.register();
    for body in ["january", "february", "march", "now"] {
        bbs.ok(&["send", "--to", &a, body]);
    }
    bbs.db()
        .execute_batch(
            "UPDATE messages SET created_at = '2026-01-10T00:00:00.000000Z' WHERE body = 'january';
             UPDATE messages SET created_at = '2026-02-10T00:00:00.000000Z' WHERE body = 'february';
             UPDATE messages SET created_at = '2026-03-10T00:00:00.000000Z' WHERE body = 'march';",
        )
        .unwrap();
    let window = |args: &[&str]| -> Vec<String> {
        bodies(&view_json(&bbs, args))
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        window(&[
            "--since",
            "2026-02-01T00:00:00Z",
            "--until",
            "2026-03-10T00:00:00Z"
        ]),
        ["february"],
        "since is inclusive, until exclusive"
    );
    assert_eq!(
        window(&["--since", "2026-02-10T01:00:00+01:00"]),
        ["february", "march", "now"]
    );
    assert_eq!(window(&["--since", "1h"]), ["now"]);
    assert_eq!(window(&["--until", "1h"]), ["january", "february", "march"]);
    // A window older than the default backlog is reachable by time.
    assert_eq!(window(&["--until", "2026-02-01T00:00:00Z"]), ["january"]);
    // --until never follows: a window bounded in the past cannot grow, and the
    // TUI is not needed to print it.
    for args in [
        &["view", "--json", "--until", "1h"][..],
        &["view", "--until", "2099-01-01T00:00:00Z"],
    ] {
        let mut child = bbs.spawn(args);
        let status = wait_exit(&mut child, 10).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("{args:?} kept following")
        });
        let out = child.wait_with_output().unwrap();
        assert!(
            status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains("march"), "{args:?}: {stdout}");
    }
    assert!(
        bbs.err(&["view", "--json", "--since", "soon"])
            .contains("invalid --since \"soon\"")
    );
    assert!(
        bbs.err(&["view", "--json", "--since", "1h", "--until", "2h"])
            .contains("must be earlier than until")
    );
}

/// Lines from a spawned `bbs view --json`, read on a thread.
fn spawn_lines(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn next_body(rx: &std::sync::mpsc::Receiver<String>) -> String {
    let line = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("view --json emitted a line");
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    value["body"].as_str().unwrap().to_string()
}

/// With `--limit 1`, each new message replaces the whole window; a burst far
/// larger than the window arrives in one change. Both must print every
/// message once and keep the stream open.
fn view_json_follow_survives_full_windows_and_bursts(mode: Mode) {
    let bbs = Bbs::with_mode("view-burst", mode);
    let (a, _ta) = bbs.register();
    bbs.ok(&["send", "--to", &a, "history"]);
    let mut child = bbs.spawn(&["view", "--json", "--limit", "1", "--from", "will"]);
    let rx = spawn_lines(&mut child);
    assert_eq!(next_body(&rx), "history");

    bbs.ok(&["send", "--to", &a, "one"]);
    assert_eq!(next_body(&rx), "one");
    bbs.ok(&["send", "--to", &a, "two"]);
    assert_eq!(next_body(&rx), "two");

    // Commit a burst in one transaction, so a single change brings all of it.
    let db = bbs.db();
    let will: i64 = db
        .query_row("SELECT id FROM humans WHERE name = 'will'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let agent: i64 = db
        .query_row("SELECT id FROM agents WHERE name = ?1", [&a], |r| r.get(0))
        .unwrap();
    let mut sql = String::from("BEGIN;");
    for i in 0..5 {
        sql.push_str(&format!(
            "INSERT INTO messages (from_kind, from_id, body, created_at)
             VALUES ('human', {will}, 'burst {i}', '2026-10-15T00:00:00.000000Z');
             INSERT INTO deliveries (message_id, recipient_kind, recipient_id)
             VALUES (last_insert_rowid(), 'agent', {agent});"
        ));
    }
    sql.push_str("COMMIT;");
    db.execute_batch(&sql).unwrap();
    // The server is only notified by its own writes; this send wakes it.
    bbs.ok(&["send", "--to", &a, "after burst"]);
    for i in 0..5 {
        assert_eq!(next_body(&rx), format!("burst {i}"));
    }
    assert_eq!(next_body(&rx), "after burst");
    assert!(child.try_wait().unwrap().is_none(), "stream ended");
    bbs.ok(&["send", "--to", &a, "still live"]);
    assert_eq!(next_body(&rx), "still live");
    let _ = child.kill();
    let _ = child.wait();
    let extra: Vec<String> = rx.try_iter().collect();
    assert!(extra.is_empty(), "duplicate or stray lines: {extra:?}");
}

fn view_json_follow_emits_each_message_once(mode: Mode) {
    let bbs = Bbs::with_mode("view-follow", mode);
    let (a, ta) = bbs.register();
    let (_b, _tb) = bbs.register();
    bbs.ok(&["send", "--to", &a, "history"]);
    // Set up a group as another human, so its notices do not match.
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["--as", "ada", "group", "register", "acme"]);
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--as", "ada", "group", "grant", "acme", &a]);
    let mut child = bbs.spawn(&["view", "--json", "--from", "will"]);
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let next = || -> serde_json::Value {
        let line = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("view --json emitted a line");
        serde_json::from_str(&line).unwrap()
    };
    assert_eq!(next()["body"], "history");

    bbs.ok(&["send", "--to", &a, "live one"]);
    assert_eq!(next()["body"], "live one");
    // Reading changes read_at and re-sends the window; nothing is repeated.
    bbs.ok(&["--token", &ta, "read"]);
    // A non-matching sender produces a snapshot but no line.
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "x"]);
    bbs.ok(&["send", "--to", &a, "live two"]);
    assert_eq!(next()["body"], "live two");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let _ = child.kill();
    let _ = child.wait();
    let extra: Vec<String> = rx.try_iter().collect();
    assert!(extra.is_empty(), "duplicate or stray lines: {extra:?}");
}

/// Write a headless config whose only harness is `command`.
fn headless_config(bbs: &Bbs, command: &[&str]) {
    let dir = bbs.home.join(".config/bbs");
    std::fs::create_dir_all(&dir).unwrap();
    let command = command
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.fake]\ncommand = [{command}]\n"
        ),
    )
    .unwrap();
}

#[test]
fn init_keeps_headless_settings() {
    let bbs = Bbs::new("init-headless");
    headless_config(&bbs, &["omp", "acp"]);
    bbs.ok(&["init", "--human", "ada"]);
    let config = std::fs::read_to_string(config_path(&bbs)).unwrap();
    assert!(config.contains("human = \"ada\""), "{config}");
    assert!(config.contains("min_turn_interval = \"0s\""), "{config}");
    assert!(config.contains("[headless.harness.fake]"), "{config}");
    assert!(config.contains("command = [\"omp\", \"acp\"]"), "{config}");
}

#[test]
fn headless_refuses_the_old_settings_file() {
    let bbs = Bbs::new("old-headless");
    headless_config(&bbs, &["true"]);
    std::fs::write(bbs.home.join(".config/bbs/headless.toml"), "").unwrap();
    let err = bbs.err(&["headless", "run", "Zed"]);
    assert!(err.contains("headless.toml is no longer read"), "{err}");
    assert!(err.contains("under [headless]"), "{err}");
}

fn headless_state(bbs: &Bbs, agent: &str) -> serde_json::Value {
    let path = bbs
        .home
        .join(".local/state/bbs/headless")
        .join(agent.to_lowercase())
        .join("state.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A `bbs headless run` child, killed if a test fails while it runs.
struct RunGuard(Option<std::process::Child>);

impl RunGuard {
    fn start(bbs: &Bbs, agent: &str) -> RunGuard {
        RunGuard::start_with(bbs, agent, &[])
    }

    fn start_with(bbs: &Bbs, agent: &str, extra: &[&str]) -> RunGuard {
        let mut args = vec![
            "headless",
            "run",
            agent,
            "--cwd",
            bbs.home.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let child = bbs
            .command(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        RunGuard(Some(child))
    }

    /// Wait for the process to exit on its own and return its output.
    fn wait(mut self) -> std::process::Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    /// Wait for the process to exit on its own; one still running after
    /// `within` is killed and fails the test.
    fn exits(mut self, within: std::time::Duration) -> std::process::Output {
        let child = self.0.as_mut().unwrap();
        let deadline = std::time::Instant::now() + within;
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("the runner was expected to exit within {within:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    /// Send `signal` to the process and return its output.
    fn stop_with(mut self, signal: &str) -> std::process::Output {
        let child = self.0.take().unwrap();
        let killed = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        child.wait_with_output().unwrap()
    }

    fn stop(self) -> std::process::Output {
        self.stop_with("-TERM")
    }
}

impl Drop for RunGuard {
    /// SIGTERM, so `run` also stops the harness's process group.
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let _ = child.wait();
        }
    }
}

/// Whether `pid` is a running (not zombie) process.
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit(')')
            .next()
            .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
    })
}

/// Poll `f` until it returns `Some`, failing after 20 seconds.
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn headless_replies_through_tools_and_cancels(mode: Mode) {
    let bbs = Bbs::with_mode("headless", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    headless_config(&bbs, &["python3", fake]);
    let config = bbs.home.join(".config/bbs/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("share_activity = false\n");
    std::fs::write(&config, text).unwrap();
    bbs.ok(&["group", "register", "crew"]);
    let (peer, tp) = bbs.register();
    bbs.ok(&["--token", &tp, "group", "join", "crew"]);
    let run = RunGuard::start(&bbs, "Zed");
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    eventually("the session", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|l| l.contains("\"event\":\"session\""))
    });
    let tz = headless_state(&bbs, "Zed")["token"]
        .as_str()
        .unwrap()
        .to_string();
    bbs.ok(&["--token", &tz, "group", "join", "crew"]);

    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "hello"]);
    let mut seen = String::new();
    let reply = eventually("the reply", || {
        seen.push_str(&bbs.ok(&["--token", &tp, "read"]));
        seen.contains("echo (").then(|| seen.clone())
    });
    assert!(
        reply.contains(&format!("from Zed to {peer}:\necho (yes)")),
        "{reply}"
    );
    // The MCP reply is visible on the board before the runner necessarily
    // consumes its event queue. Completion is the barrier for inspecting logs.
    let completed = eventually("the first completed turn", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|l| l.contains("\"event\":\"turn_end\""))
    });
    let events: Vec<String> = completed
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let permission = events
        .iter()
        .position(|event| event == "permission")
        .expect("completed turn must retain the permission event");
    let end = events.iter().position(|event| event == "turn_end").unwrap();
    assert!(
        permission < end,
        "permission must be recorded before completion: {events:?}"
    );
    let shown = bbs.ok(&["headless", "logs", "Zed"]);
    assert!(shown.contains("You are Zed"), "{shown}");
    // This harness opts out of sharing.
    assert!(shared_activity(&bbs, "Zed").is_empty());
    assert!(shown.contains("text:\nthinking about"), "{shown}");
    assert!(shown.contains("permission:"), "{shown}");

    // Only the owner may cancel; the running process stops the turn at once.
    bbs.ok(&["human", "add", "alice"]);
    assert!(
        bbs.err(&["--as", "alice", "headless", "cancel", "Zed"])
            .contains("is not your agent")
    );
    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "WAITCANCEL"]);
    // The prompt is logged before the harness gets it; wait until the harness
    // reports it, or the cancel could reach the harness first.
    eventually("the second prompt", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|l| l.contains("\"title\":\"waiting-cancel\""))
    });
    bbs.ok(&["headless", "cancel", "Zed"]);
    eventually("the cancelled turn", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|l| l.contains("\"stop_reason\":\"cancelled\""))
    });

    // A second driver for the same agent is refused before it starts a harness.
    assert!(
        bbs.err(&["headless", "run", "Zed"])
            .contains("another `bbs headless run`")
    );

    // Stopping ends the harness's commands too.
    let child = std::fs::read_to_string(bbs.home.join("fake-child.pid")).unwrap();
    assert!(running(child.trim()));
    let out = run.stop();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("\"event\":\"stop\"")
    );
    eventually("the harness's child to exit", || {
        (!running(child.trim())).then_some(())
    });
}

fn headless_process_mcp_env_routes_without_serializing_credentials(mode: Mode) {
    let bbs = Bbs::with_mode("process-mcp-env", mode);
    // A harness may use another home whose global config selects a different board.
    let child_home = bbs.home.join("harness-home");
    let child_config = child_home.join(".config/bbs/config.toml");
    std::fs::create_dir_all(child_config.parent().unwrap()).unwrap();
    std::fs::write(&child_config, "server = \"127.0.0.1:2\"\n").unwrap();
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    headless_config(&bbs, &["python3", fake]);
    let config = config_path(&bbs);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "mcp_env = \"process\"\nshare_activity = false\nmodel = \"synthetic/model\"\n\
         env = {{ HOME = {child_home:?}, BBS_HEADLESS_TOKEN = \"stale-token\", BBS_SERVER = \"127.0.0.1:1\", \
         FAKE_MCP_PROCESS_ENV = \"1\", CLAUDE_CODE_SUBPROCESS_ENV_SCRUB = \"0\" }}\n",
    ));
    std::fs::write(&config, &text).unwrap();
    bbs.ok(&["group", "register", "crew"]);
    let (_, peer_token) = bbs.register();
    bbs.ok(&["--token", &peer_token, "group", "join", "crew"]);
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    for (index, expected) in [
        "session/new",
        "session/load",
        "session/resume",
        "session/new",
    ]
    .into_iter()
    .enumerate()
    {
        if index == 2 {
            text = text.replace(
                "FAKE_MCP_PROCESS_ENV =",
                "FAKE_ACP_RESUME = \"1\", FAKE_MCP_PROCESS_ENV =",
            );
            std::fs::write(&config, &text).unwrap();
        }
        let extra: &[&str] = if index == 3 { &["--new-session"] } else { &[] };
        let run = RunGuard::start_with(&bbs, "Zed", extra);
        eventually("session method", || {
            let s = std::fs::read_to_string(bbs.home.join("fake-session-methods")).ok()?;
            (s.lines().count() == index + 1 && s.lines().last() == Some(expected)).then_some(())
        });
        if index == 0 {
            let token = headless_state(&bbs, "Zed")["token"]
                .as_str()
                .unwrap()
                .to_string();
            bbs.ok(&["--token", &token, "group", "join", "crew"]);
        }
        bbs.ok(&["--token", &peer_token, "send", "--to", "Zed", "hello"]);
        eventually("reply through inherited MCP credentials", || {
            bbs.ok(&["--token", &peer_token, "read"])
                .contains("echo (yes)")
                .then_some(())
        });
        if index == 3 {
            bbs.ok(&["--token", &peer_token, "send", "--to", "Zed", "WAITCANCEL"]);
            eventually("running turn", || {
                std::fs::read_to_string(&log)
                    .ok()
                    .filter(|s| s.contains("waiting-cancel"))
            });
            bbs.ok(&["headless", "cancel", "Zed"]);
            eventually("cancelled turn", || {
                std::fs::read_to_string(&log)
                    .ok()
                    .filter(|s| s.contains("\"stop_reason\":\"cancelled\""))
            });
        }
        let output = run.stop();
        assert!(output.status.success());
        let token = headless_state(&bbs, "Zed")["token"]
            .as_str()
            .unwrap()
            .to_string();
        for path in [
            log.clone(),
            bbs.home.join("fake-mcp-specs.jsonl"),
            bbs.home
                .join(".local/state/bbs/headless/zed/harness.stderr"),
        ] {
            assert!(!std::fs::read_to_string(path).unwrap().contains(&token));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&token));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
        let models = std::fs::read_to_string(bbs.home.join("fake-model-selections")).unwrap();
        assert_eq!(
            models.lines().collect::<Vec<_>>(),
            vec!["synthetic/model"; index + 1]
        );
        assert_eq!(
            std::fs::read_to_string(&child_config).unwrap(),
            "server = \"127.0.0.1:2\"\n"
        );
        assert!(!child_home.join(".local/state/bbs/bbs.db").exists());
    }
}

#[test]
fn headless_mcp_local_state_dir_must_be_absolute() {
    let bbs = Bbs::new("relative-mcp-state");
    let err = bbs.err(&["headless", "mcp", "--local-state-dir", "relative-board"]);
    assert!(
        err.contains("--local-state-dir must be an absolute path"),
        "{err}"
    );
    assert!(!bbs.home.join("relative-board").exists());
}

#[test]
fn headless_process_mcp_env_rejects_scrub_before_registration() {
    let bbs = Bbs::new("scrub-mcp-env");
    headless_config(&bbs, &["true"]);
    let config = config_path(&bbs);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("mcp_env = \"process\"\n");
    std::fs::write(&config, text).unwrap();
    let output = bbs
        .command(&["headless", "run", "Zed"])
        .env("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incompatible"));
    assert!(
        !bbs.home
            .join(".local/state/bbs/headless/zed/state.json")
            .exists()
    );
    assert!(!bbs.ok(&["agents"]).contains("Zed"));
}

/// The `event` of each activity row the board holds for `agent`, oldest first.
fn shared_activity(bbs: &Bbs, agent: &str) -> Vec<String> {
    bbs.db()
        .prepare(
            "SELECT a.event FROM activity a JOIN agents ag ON ag.id = a.agent_id
             WHERE ag.name = ?1 ORDER BY a.id",
        )
        .unwrap()
        .query_map([agent], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn headless_shares_activity_by_default(mode: Mode) {
    let bbs = Bbs::with_mode("share-activity", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    headless_config(&bbs, &["python3", fake]);
    bbs.ok(&["group", "register", "crew"]);
    let (_peer, tp) = bbs.register();
    bbs.ok(&["--token", &tp, "group", "join", "crew"]);
    let run = RunGuard::start(&bbs, "Zed");
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    eventually("the session", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|l| l.contains("\"event\":\"session\""))
    });
    let tz = headless_state(&bbs, "Zed")["token"]
        .as_str()
        .unwrap()
        .to_string();
    bbs.ok(&["--token", &tz, "group", "join", "crew"]);
    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "hello"]);
    eventually("the shared turn", || {
        let events = shared_activity(&bbs, "Zed");
        events.contains(&"turn_end".to_string()).then_some(())
    });
    let out = run.stop();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Stopping waits briefly for the last records, so the stop is shared too.
    let events = eventually("the shared stop", || {
        let events = shared_activity(&bbs, "Zed");
        events.contains(&"stop".to_string()).then_some(events)
    });
    for event in [
        "start", "session", "mail", "prompt", "text", "turn_end", "stop",
    ] {
        assert!(events.contains(&event.to_string()), "{event}: {events:?}");
    }
    // Raw deltas stay local; shared text appears only at its delivery ACK.
    let logged: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .filter(|e| !matches!(e.as_str(), "update" | "text"))
        .map(|e| {
            if e == "owner_delivery" {
                "text".into()
            } else {
                e
            }
        })
        .collect();
    assert_eq!(events, logged);

    // Retiring the agent deletes what it shared.
    bbs.ok(&["retire", "Zed"]);
    assert!(shared_activity(&bbs, "Zed").is_empty());
}

fn headless_codex_replies_steers_and_interrupts(mode: Mode) {
    let bbs = Bbs::with_mode("codex-app-server", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_codex.py");
    let dir = bbs.home.join(".config/bbs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.codex]\n\
             type = \"codex-app-server\"\ncommand = [\"python3\", {fake:?}]\n\
             provider = \"fakeprovider\"\n"
        ),
    )
    .unwrap();
    bbs.ok(&["group", "register", "crew"]);
    let (peer, tp) = bbs.register();
    bbs.ok(&["--token", &tp, "group", "join", "crew"]);
    let run = RunGuard::start(&bbs, "Zed");
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    let log_has = |what: &str, needle: &str| {
        eventually(what, || {
            std::fs::read_to_string(&log)
                .ok()
                .filter(|l| l.contains(needle))
        })
    };
    // A reply is sent before its turn ends. Mail sent before then would be
    // steered into the ending turn, so wait for each turn's end.
    let turns_ended = |count: usize| {
        eventually("the turn's end", || {
            let log = std::fs::read_to_string(&log).ok()?;
            (log.matches("\"event\":\"turn_end\"").count() >= count).then_some(())
        })
    };
    log_has("the session", "\"event\":\"session\"");
    let tz = headless_state(&bbs, "Zed")["token"]
        .as_str()
        .unwrap()
        .to_string();
    bbs.ok(&["--token", &tz, "group", "join", "crew"]);
    let mut seen = String::new();
    let mut reply = |needle: &str| {
        eventually("the reply", || {
            seen.push_str(&bbs.ok(&["--token", &tp, "read"]));
            seen.contains(needle).then(|| seen.clone())
        })
    };

    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "hello"]);
    let got = reply("echo");
    assert!(got.contains(&format!("from Zed to {peer}:\necho")), "{got}");
    turns_ended(1);

    // Mail that arrives during a turn joins it.
    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "WAITSTEER"]);
    // Steering needs the turn's id, which bbs has once the turn's first item is logged.
    log_has("the steerable turn", "\"title\":\"waiting-steer\"");
    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "and this"]);
    let got = reply("steered: and this");
    assert!(got.contains("steered: and this"), "{got}");
    log_has("the steer", "\"event\":\"steer\"");
    turns_ended(2);

    bbs.ok(&["--token", &tp, "send", "--to", "Zed", "WAITCANCEL"]);
    log_has("the cancellable turn", "\"title\":\"waiting-cancel\"");
    bbs.ok(&["headless", "cancel", "Zed"]);
    log_has("the interrupted turn", "\"stop_reason\":\"interrupted\"");

    let out = run.stop();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shown = bbs.ok(&["headless", "logs", "Zed"]);
    assert!(shown.contains("text:\nthinking"), "{shown}");
    assert!(!shown.contains("turn_error"), "{shown}");
}

fn headless_codex_adopts_an_acp_thread_and_never_starts_one_in_its_place(mode: Mode) {
    let bbs = Bbs::with_mode("codex-adopt", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_codex_resume.py");
    let wire = bbs.home.join("codex-wire.jsonl");
    let dir = bbs.home.join(".config/bbs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[headless]\n[headless.harness.codex]\ntype = \"codex-app-server\"\n\
             command = [\"python3\", {fake:?}, {wire:?}]\n"
        ),
    )
    .unwrap();
    let state_path = bbs.home.join(".local/state/bbs/headless/zed/state.json");
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    // Run until the session opens, then stop; the requests bbs sent.
    let session = |extra: &[&str]| -> Vec<serde_json::Value> {
        let _ = std::fs::remove_file(&wire);
        let _ = std::fs::remove_file(&log);
        let run = RunGuard::start_with(&bbs, "Zed", extra);
        eventually("the session", || {
            std::fs::read_to_string(&log)
                .ok()
                .filter(|l| l.contains("\"event\":\"session\""))
        });
        run.stop();
        std::fs::read_to_string(&wire)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let opened = |wire: &[serde_json::Value]| -> serde_json::Value {
        wire.iter()
            .find(|v| v["method"] == "thread/start" || v["method"] == "thread/resume")
            .unwrap()
            .clone()
    };
    let set_state = |harness: &str, session: &str, dynamic: bool| {
        let mut state = headless_state(&bbs, "Zed");
        state["harness"] = harness.into();
        state["session_id"] = session.into();
        match dynamic {
            true => state["dynamic_tools"] = true.into(),
            false => {
                state.as_object_mut().unwrap().remove("dynamic_tools");
            }
        }
        std::fs::write(&state_path, state.to_string()).unwrap();
    };

    // A new thread gets the tools as functions, and remembers it.
    let first = opened(&session(&[]));
    assert_eq!(first["method"], "thread/start");
    assert!(first["params"]["dynamicTools"].is_array());
    assert_eq!(headless_state(&bbs, "Zed")["dynamic_tools"], true);
    // Resuming it sends no MCP server: the thread keeps its tools.
    let again = opened(&session(&[]));
    assert_eq!(again["method"], "thread/resume");
    assert!(again["params"].get("config").is_none(), "{again}");

    // A thread opened through ACP (codex-acp), saved under the same
    // profile name: resumed with the BBS MCP server, as ACP gave it.
    set_state("codex", "acp-thread", false);
    let wire = session(&[]);
    let resume = opened(&wire);
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["threadId"], "acp-thread");
    let server = &resume["params"]["config"]["mcp_servers"]["bbs"];
    assert!(
        server["command"].as_str().unwrap().ends_with("bbs"),
        "{server}"
    );
    assert_eq!(server["args"][0], "headless");
    assert!(server["env"]["BBS_HEADLESS_TOKEN"].is_string(), "{server}");
    assert!(resume["params"].get("dynamicTools").is_none());
    assert!(!wire.iter().any(|v| v["method"] == "thread/start"));
    let state = headless_state(&bbs, "Zed");
    assert_eq!(state["session_id"], "acp-thread");
    assert!(state.get("dynamic_tools").is_none(), "{state}");

    // If that thread cannot be resumed, bbs stops; it never starts a new
    // thread in its place or forgets the saved one.
    set_state("codex", "gone-thread", false);
    let before = headless_state(&bbs, "Zed");
    let _ = std::fs::remove_file(bbs.home.join("codex-wire.jsonl"));
    let out = RunGuard::start_with(&bbs, "Zed", &[]).exits(std::time::Duration::from_secs(30));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not starting a new thread"), "{err}");
    assert_eq!(headless_state(&bbs, "Zed"), before);
    let wire_text = std::fs::read_to_string(bbs.home.join("codex-wire.jsonl")).unwrap();
    assert!(!wire_text.contains("thread/start"), "{wire_text}");

    // A session saved under another profile name, as after renaming one, is
    // adopted only when asked, and then strictly.
    set_state("codex-old", "acp-thread", false);
    let fresh = opened(&session(&[]));
    assert_eq!(fresh["method"], "thread/start", "another profile's session");
    set_state("codex-old", "acp-thread", false);
    let adopted = opened(&session(&["--adopt-session", "codex-old"]));
    assert_eq!(adopted["params"]["threadId"], "acp-thread");
    assert!(adopted["params"]["config"]["mcp_servers"]["bbs"].is_object());
    assert_eq!(headless_state(&bbs, "Zed")["harness"], "codex");
    set_state("codex-old", "gone-thread", false);
    let out = RunGuard::start_with(&bbs, "Zed", &["--adopt-session", "codex-old"])
        .exits(std::time::Duration::from_secs(30));
    assert!(!out.status.success());
    assert_eq!(headless_state(&bbs, "Zed")["session_id"], "gone-thread");
    // Asked to adopt from a profile nothing is saved under: refused.
    let out = RunGuard::start_with(&bbs, "Zed", &["--adopt-session", "nowhere"])
        .exits(std::time::Duration::from_secs(30));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no session saved under harness \"nowhere\""),
        "{err}"
    );
}

fn headless_acp_adopts_a_renamed_profiles_session_or_stops(mode: Mode) {
    let bbs = Bbs::with_mode("acp-adopt", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    headless_config(&bbs, &["python3", fake]);
    let state_path = bbs.home.join(".local/state/bbs/headless/zed/state.json");
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    let opened = |extra: &[&str]| {
        let _ = std::fs::remove_file(&log);
        let run = RunGuard::start_with(&bbs, "Zed", extra);
        let line = eventually("the session", || {
            std::fs::read_to_string(&log)
                .ok()?
                .lines()
                .find(|l| l.contains("\"event\":\"session\""))
                .map(str::to_string)
        });
        run.stop();
        line
    };
    opened(&[]);
    let saved = |session: &str| {
        let mut state = headless_state(&bbs, "Zed");
        state["harness"] = "codex".into();
        state["session_id"] = session.into();
        std::fs::write(&state_path, state.to_string()).unwrap();
    };
    // Saved under the profile's old name: adopted when asked.
    saved("acp-session");
    let line = opened(&["--adopt-session", "codex"]);
    assert!(
        line.contains("\"resumed\":true") && line.contains("acp-session"),
        "{line}"
    );
    let state = headless_state(&bbs, "Zed");
    assert_eq!(
        (&state["harness"], &state["session_id"]),
        (&"fake".into(), &"acp-session".into())
    );
    // If it cannot be resumed, the run stops and keeps the saved session.
    saved("gone-session");
    let out = RunGuard::start_with(&bbs, "Zed", &["--adopt-session", "codex"])
        .exits(std::time::Duration::from_secs(30));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not opening a new one"), "{err}");
    assert_eq!(headless_state(&bbs, "Zed")["session_id"], "gone-session");
}

fn headless_stops_after_repeated_turn_errors(mode: Mode) {
    let bbs = Bbs::with_mode("turn-errors", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    let dir = bbs.home.join(".config/bbs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[headless]\nmin_turn_interval = \"0s\"\nmax_turn_errors = 2\n\
             [headless.harness.fake]\ncommand = [\"python3\", {fake:?}]\n"
        ),
    )
    .unwrap();
    bbs.ok(&["group", "register", "crew"]);
    let (_, tp) = bbs.register();
    bbs.ok(&["--token", &tp, "group", "join", "crew"]);
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    let logged = |event: &str, count: usize| {
        eventually(event, || {
            let log = std::fs::read_to_string(&log).ok()?;
            let needle = format!("\"event\":\"{event}\"");
            (log.matches(&needle).count() >= count).then_some(())
        })
    };
    let mut errors = 0;
    let mut fail_twice = |run: RunGuard| {
        // One at a time, so each is its own turn.
        for _ in 0..2 {
            bbs.ok(&["--token", &tp, "send", "--to", "Zed", "FAILTURN"]);
            errors += 1;
            logged("turn_error", errors);
        }
        let out = run.wait();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("2 turns failed in a row"), "{stderr}");
    };

    // A new session that fails is kept: the session is not the suspect.
    let run = RunGuard::start(&bbs, "Zed");
    logged("session", 1);
    let tz = headless_state(&bbs, "Zed")["token"]
        .as_str()
        .unwrap()
        .to_string();
    bbs.ok(&["--token", &tz, "group", "join", "crew"]);
    fail_twice(run);
    assert_eq!(headless_state(&bbs, "Zed")["session_id"], "fake-session");

    // A resumed session that never completes a turn is dropped.
    let run = RunGuard::start(&bbs, "Zed");
    logged("session", 2);
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("\"resumed\":true")
    );
    fail_twice(run);
    assert!(headless_state(&bbs, "Zed")["session_id"].is_null());
}

fn headless_new_session_skips_resume(mode: Mode) {
    let bbs = Bbs::with_mode("new-session", mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");
    headless_config(&bbs, &["python3", fake]);
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    let sessions = |count: usize| {
        eventually("session", || {
            let log = std::fs::read_to_string(&log).ok()?;
            let resumed: Vec<bool> = log
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .filter(|record| record["event"] == "session")
                .map(|record| record["resumed"].as_bool().unwrap())
                .collect();
            (resumed.len() >= count).then_some(resumed)
        })
    };

    // A plain run resumes; --new-session does not, and the next run resumes again.
    for (count, extra) in [(1, &[][..]), (2, &[]), (3, &["--new-session"]), (4, &[])] {
        let run = RunGuard::start_with(&bbs, "Zed", extra);
        sessions(count);
        run.stop();
    }
    assert_eq!(sessions(4), [false, true, false, true]);
    assert_eq!(headless_state(&bbs, "Zed")["session_id"], "fake-session");
}

fn connect_is_for_humans_at_a_terminal(mode: Mode) {
    let bbs = Bbs::with_mode("connect", mode);
    let (_, token) = bbs.register();
    let err = bbs.err(&["--token", &token, "connect"]);
    assert!(err.contains("bbs connect is for humans"), "{err}");
    let err = bbs.err(&["connect"]);
    assert!(err.contains("bbs connect needs a terminal"), "{err}");
    // Bare `bbs` outside a terminal never reaches connect: usage, exit 2.
    let err = bbs.err(&[]);
    assert!(err.contains("Usage:"), "{err}");
}

#[cfg(unix)]
#[test]
fn connect_stops_reading_before_returning_the_terminal_to_the_shell() {
    let bbs = Bbs::new("connect-exit");
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/connect_exit.py"
        ))
        .arg(env!("CARGO_BIN_EXE_bbs"))
        .env("HOME", &bbs.home)
        .env("USER", "will")
        .env("TERM", "xterm-256color")
        .env_remove("BBS_SERVER")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn control_messages_are_for_owned_agents_only(mode: Mode) {
    let bbs = Bbs::with_mode("control", mode);
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    bbs.ok(&["headless", "cancel", &a]);
    let row: Option<String> = bbs
        .db()
        .query_row(
            "SELECT control FROM messages ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row.as_deref(), Some("cancel"));
    assert!(
        bbs.ok(&["--token", &ta, "read"])
            .contains("Cancel your running turn")
    );
    assert!(
        bbs.err(&["--as", "alice", "headless", "cancel", &a])
            .contains("is not your agent")
    );
    assert!(
        bbs.err(&["headless", "cancel", "@crew"])
            .contains("not a group")
    );
    assert!(
        bbs.err(&["headless", "cancel", "alice"])
            .contains("no live agent")
    );
}

fn headless_signals_during_setup_stop_the_harness(mode: Mode) {
    let bbs = Bbs::with_mode("setup-signal", mode);
    // A harness that never answers `initialize`, with a command of its own.
    headless_config(
        &bbs,
        &[
            "sh",
            "-c",
            "sleep 300 & echo $! > fake-child.pid; exec sleep 300",
        ],
    );
    let run = RunGuard::start(&bbs, "Zed");
    let pid_file = bbs.home.join("fake-child.pid");
    let child = eventually("the harness's child", || {
        std::fs::read_to_string(&pid_file)
            .ok()
            .filter(|p| !p.trim().is_empty())
    });
    assert!(running(child.trim()));
    // A closed terminal (SIGHUP) stops run like SIGTERM.
    let out = run.stop_with("-HUP");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stopped during harness setup"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    eventually("the harness's child to exit", || {
        (!running(child.trim())).then_some(())
    });
    let log =
        std::fs::read_to_string(bbs.home.join(".local/state/bbs/headless/zed/log.jsonl")).unwrap();
    assert!(log.contains("\"event\":\"stop\""), "{log}");
}

fn headless_run_takes_over_an_owned_agent(mode: Mode) {
    let bbs = Bbs::with_mode("takeover", mode);
    // A harness that exits at once: `run` fails after settling the token.
    headless_config(&bbs, &["true"]);
    let register_out = bbs.ok(&["register", "--name", "Zed"]);
    let old = register_out
        .split("--token ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    // An interactive session bound to the agent before the takeover.
    let bind = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "bbs register --name Zed" },
        "tool_response": { "stdout": register_out, "stderr": "", "interrupted": false, "isImage": false },
    });
    let compact = serde_json::json!({ "session_id": "s1", "hook_event_name": "SessionStart", "source": "compact" });
    assert!(hook(&bbs, bind).is_none());
    assert!(hook(&bbs, compact.clone()).is_some());

    // A saved token the server rejects still leads to a takeover; only other
    // failures (the server restarting, say) stop `run`.
    let dir = bbs.home.join(".local/state/bbs/headless/zed");
    std::fs::create_dir_all(&dir).unwrap();
    let saved = serde_json::json!({
        "server": bbs.server.as_ref().map(|s| s.addr.clone()),
        "token": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
    });
    std::fs::write(dir.join("state.json"), saved.to_string()).unwrap();

    let err = bbs.err(&["headless", "run", "Zed"]);
    assert!(!err.contains("checking the saved agent token"), "{err}");
    if matches!(mode, Mode::Remote) {
        // The test server runs in trust mode, where owners cannot be verified.
        assert!(err.contains("--require-human-token"), "{err}");
        return;
    }
    let new = headless_state(&bbs, "Zed")["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new, old);
    assert!(bbs.err(&["--token", &old, "whoami"]).contains("token"));
    assert_eq!(bbs.ok(&["--token", &new, "whoami"]), "Zed\n");
    // The old session no longer recovers the agent's (new) token.
    assert!(hook(&bbs, compact).is_none());

    bbs.ok(&["human", "add", "alice"]);
    bbs.register_with(&["--as", "alice", "register", "--name", "Ada"]);
    assert!(
        bbs.err(&["headless", "run", "Ada"])
            .contains("is not your agent")
    );
}

both_modes!(
    blocker_tools_and_cli_keep_versions_retries_and_answer_delivery,
    requested_names_are_validated_and_reserved,
    concurrent_registration_reserves_one_name,
    registration_records_human_from_as_config_or_user,
    cross_owner_messages_are_qualified_for_each_reader,
    init_is_idempotent,
    direct_messages_within_group,
    humans_and_agents_message_each_other,
    broadcast_reaches_members_and_humans,
    bye_retires_token_and_name,
    bye_notifies_and_leaves_groups,
    humans_retire_agents,
    stale_agents_are_retired_on_register,
    human_name_is_reserved_and_permissions_enforced,
    whoami_reports_agent_or_human,
    humans_remove_agents_from_groups,
    deleting_groups_archives_history_and_removes_access,
    group_archive_rolls_back_notice_on_failure,
    groups_carry_human_written_descriptions,
    joining_notifies_the_group,
    leaving_or_removal_notifies_the_group,
    humans_add_agents_to_groups,
    humans_grant_and_revoke_group_writers,
    agents_lists_status_and_scopes_visibility,
    json_output_is_one_object_per_line,
    active_at_writes_are_debounced,
    claude_code_hooks_bind_and_restore_identity,
    codex_hooks_bind_from_string_tool_response,
    read_wait_blocks_until_mail_arrives,
    only_one_read_wait_per_inbox,
    read_wait_fails_if_retired_while_waiting,
    view_is_for_humans_only,
    view_filters_by_group_sender_owner_and_scope,
    view_with_shows_what_a_name_sent_and_was_sent_directly,
    timers_are_set_listed_fired_and_removed,
    headless_agents_manage_timers_with_tools,
    headless_search_reads_the_corpus_and_pages_without_archiving,
    view_since_and_until_bound_history,
    view_json_follow_emits_each_message_once,
    view_json_follow_survives_full_windows_and_bursts,
    headless_replies_through_tools_and_cancels,
    headless_owner_delivery_acp,
    headless_owner_delivery_codex,
    headless_process_mcp_env_routes_without_serializing_credentials,
    headless_shares_activity_by_default,
    headless_codex_replies_steers_and_interrupts,
    headless_codex_adopts_an_acp_thread_and_never_starts_one_in_its_place,
    headless_acp_adopts_a_renamed_profiles_session_or_stops,
    headless_stops_after_repeated_turn_errors,
    headless_new_session_skips_resume,
    connect_is_for_humans_at_a_terminal,
    control_messages_are_for_owned_agents_only,
    headless_run_takes_over_an_owned_agent,
    headless_signals_during_setup_stop_the_harness,
);

#[test]
fn themes_are_local_and_redirected_pretty_logs_stay_plain() {
    let bbs = Bbs::new("theme-output");
    let dir = bbs.home.join(".local/state/bbs/headless/zed");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log.jsonl"),
        "{\"event\":\"text\",\"text\":\"**body**\"}\n{\"event\":\"error\",\"message\":\"failure\"}\n"
    ).unwrap();
    for theme in ["gruvbox-dark-medium", "terminal", "no-color"] {
        let out = bbs
            .command(&["--theme", theme, "headless", "logs", "Zed", "--pretty"])
            .env("BBS_THEME", "invalid-ignored-by-cli")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains("**body**") && text.contains("failure"),
            "{text}"
        );
        assert!(!text.contains('\x1b'));
    }
    let invalid = bbs
        .command(&["headless", "logs", "Zed", "--pretty"])
        .env("BBS_THEME", "invalid")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown theme"));
    let env = bbs
        .command(&["headless", "logs", "Zed", "--pretty"])
        .env("BBS_THEME", "terminal")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(env.status.success());
    assert!(!env.stdout.contains(&0x1b));
}

fn headless_owner_delivery_acp(mode: Mode) {
    owner_delivery(mode, "acp");
}
fn headless_owner_delivery_codex(mode: Mode) {
    owner_delivery(mode, "codex-app-server");
}

fn owner_delivery(mode: Mode, kind: &str) {
    let bbs = Bbs::with_mode(&format!("owner-delivery-{kind}"), mode);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_owner_delivery.py");
    headless_config(
        &bbs,
        &["python3", fake, if kind == "acp" { "acp" } else { "codex" }],
    );
    let config = bbs.home.join(".config/bbs/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!("type = {kind:?}\nshare_activity = false\n"));
    std::fs::write(config, text).unwrap();
    bbs.ok(&["human", "add", "alice"]);
    let prompt_file = bbs.home.join("instructions.txt");
    std::fs::write(&prompt_file, "CUSTOM INSTRUCTIONS").unwrap();
    let run = RunGuard::start_with(
        &bbs,
        "Zed",
        &["--prompt-file", prompt_file.to_str().unwrap()],
    );
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    eventually("owner-delivery session", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("\"event\":\"session\""))
    });
    bbs.ok(&["send", "--to", "Zed", "FIRST"]);
    eventually("final owner mail before turn completion", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("\"event\":\"turn_end\""))
    });
    let conn = rusqlite::Connection::open(match &bbs.server {
        Some(s) => s.state.join("bbs.db"),
        None => bbs.home.join(".local/state/bbs/bbs.db"),
    })
    .unwrap();
    let bodies = || -> Vec<String> {
        conn.prepare("SELECT body FROM messages WHERE auto_reply = 1 ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let first = bodies();
    assert_eq!(
        first.concat(),
        format!(
            " hello\n**world**\n \n\t```text\n{}\n```\nFINAL\n",
            "é".repeat(33000)
        )
    );
    assert!(first.iter().all(|s| s.len() <= 65536));
    assert!(first.iter().any(|s| s == " \n\t"));
    assert!(!first.concat().contains("FOREIGN"));
    assert!(!first.concat().contains("OLD REPLAY"));
    assert!(shared_activity(&bbs, "Zed").is_empty());
    assert!(
        bbs.ok(&["--as", "alice", "read"])
            .contains("other-recipient-still-works")
    );
    let prompts = std::fs::read_to_string(bbs.home.join("owner-prompts.jsonl")).unwrap();
    assert!(
        prompts.find("owning human is will").unwrap()
            < prompts.find("CUSTOM INSTRUCTIONS").unwrap()
    );
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.rfind("\"event\":\"owner_delivery\"").unwrap()
            < logged.rfind("\"event\":\"turn_end\"").unwrap()
    );
    assert!(logged.contains("\"mail_ids\":["));
    assert!(run.stop().status.success());
    let run = RunGuard::start(&bbs, "Zed");
    eventually("resumed session", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("\"resumed\":true"))
    });
    bbs.ok(&["send", "--to", "Zed", "INTERRUPT"]);
    eventually("partial owner message", || {
        bodies().concat().ends_with(" partial\n").then_some(())
    });
    bbs.ok(&["headless", "cancel", "Zed"]);
    eventually("interrupted tail", || {
        bodies()
            .concat()
            .ends_with(" partial\ntail\n")
            .then_some(())
    });
    assert!(run.stop().status.success());
    let prompts: Vec<String> = std::fs::read_to_string(bbs.home.join("owner-prompts.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(prompts.len(), 2);
    assert!(!prompts[1].contains("Speak naturally"));
    assert!(!bodies().concat().contains("OLD REPLAY"));
    assert!(!bodies().concat().contains("STALE"));
}

#[test]
fn headless_owner_outage_survives_restart_without_model_rerun() {
    let mut bbs = Bbs::with_mode("owner-outage", Mode::Remote);
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_owner_delivery.py");
    headless_config(&bbs, &["python3", fake, "acp"]);
    let run = RunGuard::start_with(&bbs, "Zed", &["--mid-turn", "queue"]);
    let log = bbs.home.join(".local/state/bbs/headless/zed/log.jsonl");
    eventually("outage session", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("\"event\":\"session\""))
    });
    bbs.ok(&["send", "--to", "Zed", "OUTAGE"]);
    eventually("accepted prompt", || {
        bbs.home.join("owner-prompts.jsonl").exists().then_some(())
    });
    bbs.ok(&["send", "--to", "Zed", "NEVER SUBMITTED"]);
    eventually("queued ordinary mail", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("\"queued\":true"))
    });
    let mut server = bbs.server.take().unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    std::fs::write(bbs.home.join("release-output"), "").unwrap();
    eventually("durable text while offline", || {
        std::fs::read_to_string(&log)
            .ok()
            .filter(|s| s.contains("owner delivery delayed"))
    });
    let stopped = run.stop();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let prompts = std::fs::read_to_string(bbs.home.join("owner-prompts.jsonl")).unwrap();
    assert_eq!(
        prompts.lines().count(),
        1,
        "pending delivery must block another model turn"
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .any(|s| s.contains("unprocessed") && s.contains("NEVER SUBMITTED"))
    );
    let parts = bbs.home.join(".local/state/bbs/headless/zed/owner-outbox");
    assert!(std::fs::read_dir(&parts).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("part-")
    }));
    // Restart only the synthetic loopback server, on the same board and port.
    let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["serve", "--listen", &server.addr, "--state-dir"])
        .arg(&server.state)
        .env("HOME", &bbs.home)
        .env_remove("BBS_SERVER")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.contains("listening"), "{line}");
    bbs.server = Some(Server {
        child,
        addr: server.addr.clone(),
        state: server.state.clone(),
    });
    let run = RunGuard::start(&bbs, "Zed");
    let conn = rusqlite::Connection::open(server.state.join("bbs.db")).unwrap();
    eventually("retained owner reply", || {
        let count: i64 = conn.query_row("SELECT count(*) FROM messages WHERE auto_reply=1 AND body='durable outage reply' || char(10)", [], |r| r.get(0)).unwrap();
        (count == 1).then_some(())
    });
    assert!(run.stop().status.success());
    assert_eq!(
        std::fs::read_to_string(bbs.home.join("owner-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!std::fs::read_dir(&parts).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("part-")
    }));
}

#[test]
fn serve_without_bbs_server_says_how_to_install_it() {
    // `bbs` alone, with nothing beside it and nothing on PATH.
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_bbs"));
    let dir = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
    let bbs = dir
        .path()
        .join(format!("bbs{}", std::env::consts::EXE_SUFFIX));
    // Linking avoids writable executable handles inherited by parallel test forks.
    std::fs::hard_link(binary, &bbs).unwrap();
    let out = std::process::Command::new(&bbs)
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("PATH", "")
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cargo install --locked bbs-server"), "{err}");
    assert!(!err.contains("--features"), "{err}");
    // Nothing was started or created on the way.
    assert!(!dir.path().join(".local").exists());
}

#[test]
fn serve_passes_on_only_what_follows_it() {
    // `bbs`'s own options stay with `bbs`; bbs-server sees just its own.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["--as", "ada", "--theme", "no-color", "serve", "--version"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.starts_with("bbs-server "), "{shown}");
}

#[cfg(unix)]
#[test]
fn serve_skips_a_bbs_server_it_cannot_run() {
    use std::os::unix::fs::PermissionsExt;
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_bbs"));
    let dir = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
    let bbs = dir.path().join("bbs");
    // Linking avoids writable executable handles inherited by parallel test forks.
    std::fs::hard_link(binary, &bbs).unwrap();
    // A bbs-server beside it without the execute bit is not the server.
    let fake = dir.path().join("bbs-server");
    std::fs::write(&fake, "not a program").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = std::process::Command::new(&bbs)
        .args(["serve", "--version"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not installed"));
}
