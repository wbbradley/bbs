//! Fake provider + local loopback board. No live agents, accounts or prompts.
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tonic::{Request, transport::Channel};
mod proto {
    tonic::include_proto!("bbs.v2");
}
use proto::bbs_client::BbsClient;

struct Rig {
    home: tempfile::TempDir,
    server: Child,
    runner: Option<Child>,
    addr: String,
}
impl Rig {
    async fn new(scenario: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/bbs")).unwrap();
        let mut server = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--state-dir"])
            .arg(home.path().join("board"))
            .env("HOME", home.path())
            .env_remove("BBS_SERVER")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line.split_whitespace().last().unwrap().to_string();
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_btw.py");
        let harness = "codex-app-server";
        std::fs::write(home.path().join(".config/bbs/config.toml"),format!("human = \"tester\"\n[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.fake]\ntype = {harness:?}\nshare_activity = false\ncommand = [\"python3\",{fake:?},{scenario:?}]\n")).unwrap();
        let mut client = BbsClient::connect(format!("http://{addr}")).await.unwrap();
        client
            .ensure_human(req(proto::EnsureHumanRequest {
                name: "tester".into(),
            }))
            .await
            .unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["headless", "run", "BtwTest", "--harness", "fake", "--cwd"])
            .arg(home.path())
            .env("HOME", home.path())
            .env("BBS_SERVER", &addr)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let rig = Self {
            home,
            server,
            runner: Some(runner),
            addr,
        };
        rig.wait_report(|r| r.side_supported == Some(true)).await;
        rig
    }
    async fn client(&self) -> BbsClient<Channel> {
        BbsClient::connect(format!("http://{}", self.addr))
            .await
            .unwrap()
    }
    async fn send(&self, body: &str, control: Option<proto::Control>) {
        self.client()
            .await
            .send(req(proto::SendRequest {
                to: "BtwTest".into(),
                body: body.into(),
                control: control.map(Into::into),
            }))
            .await
            .unwrap();
    }
    async fn report(&self) -> proto::SideReportResponse {
        self.client()
            .await
            .side_report(req(proto::SideReportRequest {
                agent: "BtwTest".into(),
                generation: None,
            }))
            .await
            .unwrap()
            .into_inner()
    }
    async fn wait_report(
        &self,
        p: impl Fn(&proto::SideReportResponse) -> bool,
    ) -> proto::SideReportResponse {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Ok(r) = self
                    .client()
                    .await
                    .side_report(req(proto::SideReportRequest {
                        agent: "BtwTest".into(),
                        generation: None,
                    }))
                    .await
                {
                    let r = r.into_inner();
                    if p(&r) {
                        return r;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("side report did not arrive")
    }
    fn wire(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home.path().join("btw-wire.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    async fn wait_wire(&self, p: impl Fn(&[Value]) -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !p(&self.wire()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("provider boundary missing");
    }
    fn stop(&mut self) {
        if let Some(mut runner) = self.runner.take()
            && runner.try_wait().unwrap().is_none()
        {
            Command::new("kill")
                .args(["-TERM", &runner.id().to_string()])
                .status()
                .unwrap();
            assert!(runner.wait().unwrap().success());
        }
    }
    fn crash(&mut self) {
        let pid = self
            .wire()
            .into_iter()
            .rev()
            .find(|v| v["kind"] == "provider_pid")
            .unwrap()["pid"]
            .as_u64()
            .unwrap();
        let mut runner = self.runner.take().unwrap();
        runner.kill().unwrap();
        runner.wait().unwrap();
        // This exact fake process belongs to this rig; no global process matching.
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
    }
    fn restart(&mut self) {
        self.runner = Some(
            Command::new(env!("CARGO_BIN_EXE_bbs"))
                .args(["headless", "run", "BtwTest", "--harness", "fake", "--cwd"])
                .arg(self.home.path())
                .env("HOME", self.home.path())
                .env("BBS_SERVER", &self.addr)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    async fn ask(&self) -> i64 {
        self.client()
            .await
            .ask_side(req(proto::AskSideRequest {
                agent: "BtwTest".into(),
                question: "A PRIVATE SIDE QUESTION".into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .id
    }
    fn messages(&self) -> i64 {
        let conn = rusqlite::Connection::open(self.home.path().join("board/bbs.db")).unwrap();
        conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}
fn req<T>(msg: T) -> Request<T> {
    let mut r = Request::new(msg);
    for (k, v) in [
        ("bbs-protocol-epoch", "2"),
        ("bbs-human", "tester"),
        ("bbs-client-version", env!("CARGO_PKG_VERSION")),
    ] {
        r.metadata_mut().insert(k, v.parse().unwrap());
    }
    r
}

#[tokio::test]
async fn side_tools_and_parent_tools_run_without_chat_or_context_leakage() {
    let mut r = Rig::new("tools").await;
    r.send("HOLD MAIN", None).await;
    let before = r
        .wait_report(|r| r.usage.is_some_and(|u| u.used_tokens == Some(40)))
        .await;
    let messages = r.messages();
    let id = r.ask().await;
    let result = r
        .wait_report(|r| {
            r.side
                .as_ref()
                .is_some_and(|s| s.id == id && s.status() == proto::SideStatus::SideCompleted)
        })
        .await;
    assert_eq!(
        result.side.unwrap().answer.as_deref(),
        Some("side prefix\nside final\n")
    );
    assert_eq!(result.usage, before.usage);
    assert_eq!(result.reported_at, before.reported_at);
    assert!(result.pending_input.is_none());
    assert_eq!(
        r.messages(),
        messages,
        "side Q/A and tool results never become mail"
    );
    for id in ["tool", "parent-tool"] {
        let reply = r.wire().into_iter().find(|v| v["id"] == id).unwrap();
        assert_eq!(reply["response"]["result"]["success"], true);
    }
    assert!(
        !r.wire()
            .iter()
            .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "parent")
    );
    r.send("cancel parent", Some(proto::Control::Cancel)).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mail = r
                .client()
                .await
                .read(req(proto::ReadRequest::default()))
                .await
                .unwrap()
                .into_inner()
                .messages;
            if let Some(m) = mail.into_iter().find(|m| m.auto_reply == Some(true)) {
                assert!(
                    !m.body.contains("side prefix") && !m.body.contains("PRIVATE SIDE QUESTION")
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    r.stop();
}
#[tokio::test]
async fn child_prerequisite_failure_preserves_parent_usage_and_disables_side_capability() {
    let r = Rig::new("no-goal").await;
    r.send("HOLD MAIN", None).await;
    let before = r
        .wait_report(|r| r.usage.is_some_and(|u| u.used_tokens == Some(40)))
        .await;
    let id = r.ask().await;
    let result = r
        .wait_report(|r| {
            r.side
                .as_ref()
                .is_some_and(|s| s.id == id && s.status() == proto::SideStatus::SideFailed)
                && r.side_supported == Some(false)
        })
        .await;
    assert_eq!(result.usage, before.usage);
    assert_eq!(result.reported_at, before.reported_at);
    assert!(
        !r.wire()
            .iter()
            .any(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
    );
    assert_eq!(
        r.wire()
            .iter()
            .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "parent")
            .count(),
        1
    );
}
#[tokio::test]
async fn owner_cancel_is_independent_and_restart_never_replays_a_claimed_question() {
    let mut r = Rig::new("hold").await;
    r.send("HOLD MAIN", None).await;
    r.wait_wire(|w| w.iter().any(|v| v["kind"] == "parent_active"))
        .await;
    let id = r.ask().await;
    r.wait_report(|r| {
        r.side
            .as_ref()
            .is_some_and(|s| s.id == id && s.answer.as_deref() == Some("side prefix\n"))
    })
    .await;
    r.client()
        .await
        .cancel_side(req(proto::CancelSideRequest { id }))
        .await
        .unwrap();
    r.wait_wire(|w| {
        w.iter()
            .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "child")
    })
    .await;
    let cancelled = r.report().await.side.unwrap();
    assert_eq!(cancelled.status(), proto::SideStatus::SideCancelled);
    assert_eq!(cancelled.answer.as_deref(), Some("side prefix\n"));
    assert!(
        !r.wire()
            .iter()
            .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "parent")
    );
    let second = r.ask().await;
    r.wait_report(|r| {
        r.side
            .as_ref()
            .is_some_and(|s| s.id == second && s.answer.is_some())
    })
    .await;
    let count = r
        .wire()
        .iter()
        .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
        .count();
    r.crash();
    r.restart();
    let restarted = r
        .wait_report(|r| {
            r.side
                .as_ref()
                .is_some_and(|s| s.id == second && s.status() == proto::SideStatus::SideInterrupted)
                && r.side_supported == Some(true)
        })
        .await;
    assert_eq!(
        restarted.side.unwrap().answer.as_deref(),
        Some("side prefix\n")
    );
    assert_eq!(
        r.wire()
            .iter()
            .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
            .count(),
        count
    );
}
