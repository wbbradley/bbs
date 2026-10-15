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
            assert_eq!(report["schema_version"], 8);
            assert_eq!(report["config_format"], 1);
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
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "will", "hi"])
            .contains("cannot message humans")
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

fn humans_message_agents_and_humans_but_agents_cannot_dm_humans(mode: Mode) {
    let bbs = Bbs::with_mode("human", mode);
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "will", "need input"])
            .contains("through your harness")
    );
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
    let out = bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(out.contains("1 recipients"), "{out}"); // humans are not counted for agents

    assert!(
        bbs.ok(&["--token", &tb, "read"])
            .contains(&format!("from {a} to @acme:\nstandup"))
    );
    // A has only B's join notice, not its own broadcast.
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
            .contains(r#""name":"plain","members":[],"description":"-1 means off"}"#)
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
    assert!(
        seen_by_a
            .lines()
            .all(|l| l.split_whitespace().nth(1) != Some("human")),
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
    for token in [&ta, &tb] {
        bbs.ok(&["--token", token, "group", "join", "acme"]);
    }
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
            serde_json::json!({"name": "acme", "members": [b], "description": null}),
            serde_json::json!({"name": "empty", "members": [], "description": null}),
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
    db.execute_batch(include_str!("../migrations/0001_init.sql"))
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
            "from_kind": "agent", "from_owner": "will", "to": null,
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
    bbs.ok(&["group", "register", "acme"]);
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "x"]);
    bbs.ok(&["send", "--to", &a, "live two"]);
    assert_eq!(next()["body"], "live two");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let _ = child.kill();
    let _ = child.wait();
    let extra: Vec<String> = rx.try_iter().collect();
    assert!(extra.is_empty(), "duplicate or stray lines: {extra:?}");
}

both_modes!(
    requested_names_are_validated_and_reserved,
    concurrent_registration_reserves_one_name,
    registration_records_human_from_as_config_or_user,
    cross_owner_messages_are_qualified_for_each_reader,
    init_is_idempotent,
    direct_messages_within_group,
    humans_message_agents_and_humans_but_agents_cannot_dm_humans,
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
    view_since_and_until_bound_history,
    view_json_follow_emits_each_message_once,
    view_json_follow_survives_full_windows_and_bursts,
);
