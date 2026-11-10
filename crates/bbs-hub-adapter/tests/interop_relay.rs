//! Live interop with Hub's Go implementation, through Hub's own local test
//! relay (`scripts/interop-relay.sh`): a BBS server and this adapter on one
//! side, a Go Hub that is the relay and the home of namespace `hub` on the
//! other. It needs Go and a Hub checkout, which CI does not have, so it is
//! opt-in:
//!
//! ```sh
//! BBS_HUB_CHECKOUT=$HOME/src/hub cargo test -p bbs-hub-adapter \
//!     --test interop_relay -- --ignored --nocapture
//! ```
//!
//! Everything runs on loopback in temporary directories; the script builds
//! Hub into its own state directory.

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bbs_core::{backend, db, proto, service};
use bbs_hub_adapter::store::Store;
use bbs_hub_adapter::{Adapter, Options, Settings, keys};

const WAIT: Duration = Duration::from_secs(30);

/// The relay script's process group, stopped on drop.
struct Relay {
    child: Child,
    dir: PathBuf,
    addr: String,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.child.id())])
            .status();
        let _ = self.child.wait();
    }
}

fn start_relay(checkout: &Path, dir: &Path) -> Relay {
    use std::os::unix::process::CommandExt as _;
    let listen = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut child = Command::new("bash")
        .arg(checkout.join("scripts/interop-relay.sh"))
        .args([
            "--dir",
            dir.to_str().unwrap(),
            "--listen",
            &listen.to_string(),
        ])
        .args(["--peer-namespace", "bbs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .expect("bash runs the relay script");
    let stdout = child.stdout.take().unwrap();
    let mut addr = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if !line.contains("TOKEN") {
            eprintln!("relay: {line}");
        }
        if let Some(rest) = line.trim().strip_prefix("address:") {
            addr = rest.split_whitespace().next().map(str::to_string);
        }
        if line.starts_with("Ctrl-C stops") {
            break;
        }
    }
    Relay {
        child,
        dir: dir.to_path_buf(),
        addr: addr.expect("the relay prints its address"),
    }
}

impl Relay {
    /// Run the Hub CLI as will@hub.
    fn hub(&self, args: &[&str]) -> String {
        self.hub_with("will.token", args)
    }

    /// Run the Hub CLI as Hub's administrator.
    fn admin(&self, args: &[&str]) -> String {
        self.hub_with("admin.token", args)
    }

    fn hub_with(&self, token_file: &str, args: &[&str]) -> String {
        let token = std::fs::read_to_string(self.dir.join(token_file)).unwrap();
        let out = Command::new(self.dir.join("hub"))
            .args(args)
            .env("HUB_ADDR", &self.addr)
            .env("HUB_TOKEN", token.trim())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "hub {args:?}: {text}");
        text
    }
}

/// The message with `body` in will@hub's inbox, as `hub read --json` shows
/// it, once it arrives. Reading consumes will's other unread mail.
async fn hub_message(relay: &Relay, body: &str) -> serde_json::Value {
    let mut found = None;
    eventually(&format!("Hub delivers {body:?} to will"), || {
        found = relay
            .hub(&["read", "--json"])
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|m| m["body"] == body);
        found.is_some()
    })
    .await;
    found.unwrap()
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    while !ok() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Go and a Hub checkout: set BBS_HUB_CHECKOUT and pass --ignored"]
async fn messages_and_directory_round_trip_with_go_hub() {
    let checkout = PathBuf::from(
        std::env::var_os("BBS_HUB_CHECKOUT")
            .expect("set BBS_HUB_CHECKOUT to a Hub checkout (with scripts/interop-relay.sh)"),
    );
    let tmp = tempfile::tempdir().unwrap();
    let relay = tokio::task::spawn_blocking({
        let (checkout, dir) = (checkout.clone(), tmp.path().join("relay"));
        move || start_relay(&checkout, &dir)
    })
    .await
    .unwrap();

    // BBS: human will, agent will/scout, integration hub, publishing.
    let bbs_dir = tmp.path().join("bbs");
    std::fs::create_dir_all(&bbs_dir).unwrap();
    let mut conn = db::open(&bbs_dir.join("bbs.db")).unwrap();
    db::migrate_in(&mut conn, Some("bbs")).unwrap();
    service::ensure_human(
        &mut conn,
        proto::EnsureHumanRequest {
            name: "will".into(),
        },
    )
    .unwrap();
    let agent = service::register(
        &mut conn,
        &service::Caller {
            human: Some("will".into()),
            ..Default::default()
        },
        proto::RegisterRequest {
            name: Some("scout".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .token;
    let token = service::integration::admin_add(&mut conn, "hub").unwrap();
    service::integration::admin_publish(&mut conn, "hub", true).unwrap();
    drop(conn);
    let free = || -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    };
    let (main, integration) = (free(), free());
    let serve_dir = bbs_dir.clone();
    let bbs = tokio::spawn(async move {
        bbs_server::serve::run(
            main,
            None,
            Some(integration),
            &serve_dir,
            Some("bbs"),
            false,
            bbs_core::protocol::Policy::default(),
            service::TimerLimits::default(),
        )
        .await
    });

    // The adapter, with the installation key and credential the script made.
    let key = keys::read_key(&relay.dir.join("bbs-install.key")).unwrap();
    let (credential, _) =
        keys::read_credential(&relay.dir.join("bbs-install.cred"), "bbs", "bbs-1", &key).unwrap();
    let store = tmp.path().join("adapter/adapter.db");
    let adapter = Adapter::open(
        Settings {
            namespace: "bbs".into(),
            installation_id: "bbs-1".into(),
            integration_id: "hub".into(),
            bbs_endpoint: format!("http://{integration}"),
            relay_endpoint: format!("http://{}", relay.addr),
            token,
            token_file: None,
            key,
            credential,
        },
        &store,
        Options::default(),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(adapter.run(async {
        let _ = stopped.await;
    }));
    eventually("the route and BBS's catalog", || {
        let s = Store::open_read_only(&store).unwrap();
        s.session()
            .unwrap()
            .is_some_and(|(_, _, ready, ended)| ready && !ended)
            && s.catalog_revision().unwrap().is_some()
    })
    .await;

    // Hub's directory resolves BBS's agent through the relay and adapter.
    let dir = relay.hub(&["dir", "--namespace", "bbs", "--id", "will/scout"]);
    assert!(dir.contains("will/scout"), "{dir}");

    // Go to BBS.
    relay.hub(&["send", "id:will/scout@bbs", "hello from go"]);
    let bbs_db = bbs_dir.join("bbs.db");
    let count = |body: &str| -> i64 {
        db::open(&bbs_db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM messages WHERE body = ?1",
                [body],
                |r| r.get(0),
            )
            .unwrap()
    };
    eventually("BBS imports Go's message", || count("hello from go") == 1).await;

    // BBS's top-level IDs answer as their own aliases, its agents' names do not.
    relay.hub(&["send", "will@bbs", "hello by alias"]);
    eventually("BBS imports the message sent by alias", || {
        count("hello by alias") == 1
    })
    .await;
    let dir = relay.hub(&["dir", "--namespace", "bbs", "--alias", "scout"]);
    assert!(!dir.contains("will/scout"), "{dir}");

    // BBS to Go, and Go's reply back to BBS's agent.
    let mut remote = backend::Backend::Remote(
        backend::Remote::connect(
            &main.to_string(),
            service::Caller {
                token: Some(agent),
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let sent = remote
        .send(proto::SendRequest {
            to: "id:will@hub".into(),
            body: "hello from bbs".into(),
            control: None,
            reply_to: None,
        })
        .await
        .unwrap()
        .id
        .unwrap();
    let outbox = || {
        db::integration::outbox_state(&db::open(&bbs_db).unwrap(), sent)
            .unwrap()
            .unwrap_or_default()
            .0
    };
    eventually("Hub receives BBS's message", || outbox() == "received").await;
    let read = relay.hub(&["read", "--json"]);
    assert!(read.contains("hello from bbs"), "{read}");
    let origin: String = db::open(&bbs_db)
        .unwrap()
        .query_row(
            "SELECT origin_message_id FROM messages WHERE id = ?1",
            [sent],
            |r| r.get(0),
        )
        .unwrap();
    relay.hub(&["reply", &format!("bbs:{origin}"), "a reply from go"]);
    eventually("BBS imports the reply", || count("a reply from go") == 1).await;
    let reply_to: String = db::open(&bbs_db)
        .unwrap()
        .query_row(
            "SELECT in_reply_to_message_id FROM messages WHERE body = 'a reply from go'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply_to, origin);

    // BBS replies to Go's message: to its exact sender, answering it.
    let (go_id, go_origin): (i64, String) = db::open(&bbs_db)
        .unwrap()
        .query_row(
            "SELECT id, origin_message_id FROM messages WHERE body = 'hello from go'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let replied = remote
        .send(proto::SendRequest {
            body: "bbs answers go".into(),
            reply_to: Some(go_id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(replied.to, "id:will@hub");
    let hub_reply = hub_message(&relay, "bbs answers go").await;
    assert_eq!(hub_reply["in_reply_to"], format!("hub:{go_origin}"));
    assert_eq!(hub_reply["to"], "will@hub");

    // A Hub group with a BBS member: BBS gets a copy of will's message to
    // the group, shown as from will via the group, and a reply reaches will
    // directly, answering his message to the group.
    relay.admin(&["entity", "register", "release", "--kind", "group"]);
    relay.hub(&["group", "join", "release"]);
    relay.admin(&["group", "add", "release", "id:will/scout@bbs"]);
    relay.hub(&["send", "release", "release notes"]);
    eventually("BBS imports the group copy", || count("release notes") == 1).await;
    let mail = remote
        .read(proto::ReadRequest::default())
        .await
        .unwrap()
        .messages;
    let copy = mail.iter().find(|m| m.body == "release notes").unwrap();
    assert_eq!(copy.from_name, "id:will@hub");
    assert_eq!(copy.from_kind(), proto::Kind::Remote);
    assert_eq!(copy.via.as_deref(), Some("id:release@hub"));
    assert_eq!(copy.reported, None);
    assert_eq!(copy.group_notice, None);
    // Adding scout was announced to the group: a notice, with no sender to
    // reply to.
    let notice = mail
        .iter()
        .find(|m| m.group_notice == Some(true))
        .expect("the membership notice");
    assert_eq!(notice.from_name, "id:release@hub");
    let err = remote
        .send(proto::SendRequest {
            body: "to a notice".into(),
            reply_to: Some(notice.id),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("a group notice has no sender to reply to"),
        "{err:#}"
    );
    let original: String = db::open(&bbs_db)
        .unwrap()
        .query_row(
            "SELECT group_copy_namespace || ':' || group_copy_message_id FROM messages
             WHERE id = ?1",
            [copy.id],
            |r| r.get(0),
        )
        .unwrap();
    let replied = remote
        .send(proto::SendRequest {
            body: "bbs answers the group copy".into(),
            reply_to: Some(copy.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(replied.to, "id:will@hub");
    let hub_reply = hub_message(&relay, "bbs answers the group copy").await;
    assert_eq!(hub_reply["in_reply_to"], original);
    assert_eq!(hub_reply["to"], "will@hub");
    assert_eq!(hub_reply["from"], "will/scout@bbs");
    assert!(hub_reply.get("via").is_none(), "{hub_reply}");

    // BBS's directory of namespace hub, through the adapter and the relay,
    // signed by Go Hub and verified here.
    let listing = remote
        .remote_directory(proto::RemoteDirectoryRequest {
            namespace: "hub".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let found: Vec<_> = listing
        .entities
        .iter()
        .map(|e| (e.entity_id.as_str(), e.kind.as_str()))
        .collect();
    assert!(found.contains(&("will", "human")), "{found:?}");
    assert!(found.contains(&("echo", "agent")), "{found:?}");
    assert!(listing.entities.iter().all(|e| e.namespace == "hub"));
    let will = remote
        .remote_directory(proto::RemoteDirectoryRequest {
            namespace: "hub".into(),
            alias: "will".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let ids: Vec<_> = will.entities.iter().map(|e| e.entity_id.as_str()).collect();
    assert_eq!(ids, ["will"]);
    // A namespace Hub does not know is an error, not an empty directory.
    let err = remote
        .remote_directory(proto::RemoteDirectoryRequest {
            namespace: "nowhere".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    let status = err.downcast_ref::<tonic::Status>().expect("a status");
    assert_eq!(status.code(), tonic::Code::NotFound, "{err:#}");
    assert!(
        err.to_string().starts_with("namespace_unknown: "),
        "{err:#}"
    );

    let _ = stop.send(());
    running.await.unwrap().unwrap();
    bbs.abort();
    drop(relay);
}
