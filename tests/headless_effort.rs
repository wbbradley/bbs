//! Loopback board + fake providers; no external accounts or model calls.
use bbs_core::proto;
use proto::{EffortReport, bbs_client::BbsClient};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tonic::{Request, transport::Channel};

struct Rig {
    home: PathBuf,
    server: Child,
    runner: Option<Child>,
    addr: String,
}
impl Rig {
    async fn new(kind: &str, test: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("bbs-effort-{}-{kind}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".config/bbs")).unwrap();
        let mut server = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--state-dir"])
            .arg(home.join("board"))
            .env("HOME", &home)
            .env_remove("BBS_SERVER")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line.split_whitespace().last().unwrap().to_string();
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_effort.py");
        let harness_type = if kind == "acp" {
            "acp"
        } else {
            "codex-app-server"
        };
        std::fs::write(home.join(".config/bbs/config.toml"), format!(
            "human = \"tester\"\n[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.fake]\ntype = {harness_type:?}\nshare_activity = false\ncommand = [\"python3\", {fake:?}, {kind:?}, {test:?}]\n"
        )).unwrap();
        let mut rig = Self {
            home,
            server,
            runner: None,
            addr,
        };
        rig.client()
            .await
            .ensure_human(Self::request(proto::EnsureHumanRequest {
                name: "tester".into(),
            }))
            .await
            .unwrap();
        rig.start();
        rig.report(|r| {
            r.state
                .as_ref()
                .is_some_and(|s| s.current.as_deref() == Some("low"))
        })
        .await;
        rig
    }
    fn start(&mut self) {
        self.runner = Some(
            Command::new(env!("CARGO_BIN_EXE_bbs"))
                .args([
                    "headless",
                    "run",
                    "EffortTest",
                    "--harness",
                    "fake",
                    "--cwd",
                ])
                .arg(&self.home)
                .env("HOME", &self.home)
                .env("BBS_SERVER", &self.addr)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.runner.take() {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            child.wait().unwrap();
        }
    }
    async fn client(&self) -> BbsClient<Channel> {
        BbsClient::connect(format!("http://{}", self.addr))
            .await
            .unwrap()
    }
    fn request<T>(msg: T) -> Request<T> {
        let mut r = Request::new(msg);
        for (key, value) in [
            ("bbs-protocol-epoch", "2"),
            ("bbs-human", "tester"),
            ("bbs-client-version", env!("CARGO_PKG_VERSION")),
        ] {
            r.metadata_mut().insert(key, value.parse().unwrap());
        }
        r
    }
    async fn send(&self, body: &str, control: Option<proto::Control>) -> i64 {
        self.client()
            .await
            .send(Self::request(proto::SendRequest {
                to: "EffortTest".into(),
                body: body.into(),
                control: control.map(Into::into),
            }))
            .await
            .unwrap()
            .into_inner()
            .id
            .unwrap()
    }
    async fn report(&self, predicate: impl Fn(&EffortReport) -> bool) -> EffortReport {
        let mut client = self.client().await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let agents = client
                    .list_agents(Self::request(proto::ListAgentsRequest { all: true }))
                    .await
                    .unwrap()
                    .into_inner()
                    .agents;
                if let Some(report) = agents
                    .into_iter()
                    .find(|a| a.name == "EffortTest")
                    .and_then(|a| a.effort)
                    && predicate(&report)
                {
                    return report;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("effort report did not arrive")
    }
    fn wire(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home.join("effort-wire.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    async fn wait_wire(&self, predicate: impl Fn(&[Value]) -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !predicate(&self.wire()) {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("fake provider did not receive expected input");
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

async fn lifecycle(kind: &str, scenario: &str) {
    let mut r = Rig::new(kind, scenario).await;
    let id = r.send("high", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.as_ref().is_some_and(|p| p.request_id == id))
        .await;
    assert!(
        !r.wire().iter().any(|v| v["kind"] == "attempt"),
        "control must not create a prompt"
    );
    r.send("FIRST", None).await;
    let report = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id))
        .await;
    assert_eq!(report.state.unwrap().current.as_deref(), Some("high"));
    assert_eq!(
        report.answered.unwrap().result,
        proto::effort_outcome::Result::Applied as i32
    );
    r.wait_wire(|v| {
        v.iter()
            .any(|v| v["kind"] == "delivered" && v["text"].as_str().unwrap().contains("FIRST"))
    })
    .await;
    r.send("HOLD", None).await;
    r.wait_wire(|v| v.iter().any(|v| v["kind"] == "held")).await;
    let id2 = r.send("low", Some(proto::Control::SetEffort)).await;
    let queued = r
        .report(|s| s.pending.as_ref().is_some_and(|p| p.request_id == id2))
        .await;
    assert_eq!(queued.state.unwrap().current.as_deref(), Some("high"));
    r.send("AFTER_HOLD", None).await; // No steering across a pending effort change.
    r.send("cancel", Some(proto::Control::Cancel)).await;
    let applied = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id2))
        .await;
    assert_eq!(applied.state.unwrap().current.as_deref(), Some("low"));
    // Release the second prompt, whose body contains HOLD too.
    r.wait_wire(|v| v.iter().filter(|v| v["kind"] == "held").count() == 2)
        .await;
    r.send("cancel", Some(proto::Control::Cancel)).await;
    r.wait_wire(|v| {
        v.iter()
            .any(|v| v["kind"] == "delivered" && v["text"].as_str().unwrap().contains("AFTER_HOLD"))
    })
    .await;
    r.stop();
    r.start();
    let resumed = r
        .report(|s| {
            s.state
                .as_ref()
                .is_some_and(|e| e.current.as_deref() == Some("medium"))
        })
        .await;
    assert!(resumed.pending.is_none());
    assert!(resumed.answered.is_none());
    r.send("METADATA", None).await;
    r.wait_wire(|v| {
        v.iter()
            .any(|v| v["kind"] == "delivered" && v["text"].as_str().unwrap().contains("METADATA"))
    })
    .await;
    let metadata = r
        .report(|s| {
            s.state
                .as_ref()
                .is_some_and(|s| s.current.as_deref() == Some("high"))
        })
        .await;
    assert!(metadata.state.unwrap().supported);
    assert!(
        metadata.answered.is_none(),
        "provider metadata is not a control ACK"
    );
    let log = std::fs::read_to_string(
        r.home
            .join(".local/state/bbs/headless/efforttest/log.jsonl"),
    )
    .unwrap();
    assert!(!log.contains("OLD REPLAY"));
}

#[tokio::test]
async fn codex_next_turn_and_resume() {
    lifecycle("codex", "lifecycle").await;
}
#[tokio::test]
async fn acp_next_turn_and_replay_metadata() {
    lifecycle("acp", "lifecycle").await;
}

async fn rejection(kind: &str) {
    let r = Rig::new(kind, "rejection").await;
    let id = r.send("max", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.is_some()).await;
    r.send("PRESERVE THIS MAIL", None).await;
    let rejected = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id))
        .await;
    assert_eq!(
        rejected.answered.unwrap().result,
        proto::effort_outcome::Result::Failed as i32
    );
    assert_eq!(rejected.state.unwrap().current.as_deref(), Some("low"));
    r.wait_wire(|v| {
        v.iter().any(|v| {
            v["kind"] == "delivered" && v["text"].as_str().unwrap().contains("PRESERVE THIS MAIL")
        })
    })
    .await;
    assert_eq!(
        r.wire().iter().filter(|v| v["kind"] == "delivered").count(),
        1
    );
    let invalid = r
        .send("not-advertised", Some(proto::Control::SetEffort))
        .await;
    let failed = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == invalid))
        .await;
    assert_eq!(
        failed.answered.unwrap().result,
        proto::effort_outcome::Result::Failed as i32
    );
}
#[tokio::test]
async fn codex_rejected_override_preserves_mail() {
    rejection("codex").await;
}
#[tokio::test]
async fn acp_rejected_setter_preserves_mail() {
    rejection("acp").await;
}

#[tokio::test]
async fn acp_clamp_is_not_acknowledgment_of_requested_value() {
    let r = Rig::new("acp", "clamp").await;
    let id = r.send("xhigh", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.is_some()).await;
    r.send("CLAMP", None).await;
    let report = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id))
        .await;
    assert_eq!(report.state.unwrap().current.as_deref(), Some("high"));
    let outcome = report.answered.unwrap();
    assert_eq!(outcome.result, proto::effort_outcome::Result::Failed as i32);
    assert!(outcome.detail.contains("provider returned"));
}

#[tokio::test]
async fn delayed_acp_ack_does_not_clear_new_pending() {
    let r = Rig::new("acp", "supersede").await;
    let first = r.send("low", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.as_ref().is_some_and(|p| p.request_id == first))
        .await;
    r.send("DELAY", None).await;
    r.wait_wire(|v| {
        v.iter()
            .any(|v| v["kind"] == "setting" && v["value"] == "low")
    })
    .await;
    let second = r.send("high", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.as_ref().is_some_and(|p| p.request_id == second))
        .await;
    r.wait_wire(|v| v.iter().any(|v| v["kind"] == "delivered"))
        .await;
    let still_pending = r
        .report(|s| s.pending.as_ref().is_some_and(|p| p.request_id == second))
        .await;
    assert!(
        !still_pending
            .answered
            .is_some_and(|a| a.request_id == second)
    );
    r.send("NEXT", None).await;
    let done = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == second))
        .await;
    assert_eq!(done.state.unwrap().current.as_deref(), Some("high"));
}

#[tokio::test]
async fn codex_disconnected_start_does_not_retry_mail_or_confirm_effort() {
    let r = Rig::new("codex", "disconnect").await;
    let id = r.send("xhigh", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.is_some()).await;
    r.send("MAYBE ACCEPTED", None).await;
    let report = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id))
        .await;
    assert_eq!(
        report.answered.unwrap().result,
        proto::effort_outcome::Result::Unknown as i32
    );
    assert!(report.state.unwrap().current.is_none());
    assert_eq!(
        r.wire().iter().filter(|v| v["kind"] == "attempt").count(),
        1
    );
    let log = std::fs::read_to_string(
        r.home
            .join(".local/state/bbs/headless/efforttest/log.jsonl"),
    )
    .unwrap();
    assert!(
        log.lines()
            .any(|s| s.contains("delivery_uncertain") && s.contains("MAYBE ACCEPTED"))
    );
}

async fn timeout_boundary(kind: &str) {
    let mut r = Rig::new(kind, "timeout").await;
    let id = r.send("high", Some(proto::Control::SetEffort)).await;
    r.report(|s| s.pending.is_some()).await;
    r.send("FIRST TIMEOUT MAIL", None).await;
    r.wait_wire(|v| {
        v.iter()
            .any(|v| v["kind"] == if kind == "acp" { "setting" } else { "attempt" })
    })
    .await;
    r.send("QUEUED NEVER SUBMITTED", None).await;
    let report = r
        .report(|s| s.answered.as_ref().is_some_and(|a| a.request_id == id))
        .await;
    assert_eq!(
        report.answered.unwrap().result,
        proto::effort_outcome::Result::Unknown as i32
    );
    assert!(report.state.unwrap().current.is_none());
    tokio::time::timeout(Duration::from_secs(10), async {
        while r.runner.as_mut().unwrap().try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let attempts = r.wire().iter().filter(|v| v["kind"] == "attempt").count();
    assert_eq!(attempts, if kind == "acp" { 0 } else { 1 });
    assert!(!r.wire().iter().any(|v| v["kind"] == "delivered"));
    let log = std::fs::read_to_string(
        r.home
            .join(".local/state/bbs/headless/efforttest/log.jsonl"),
    )
    .unwrap();
    assert!(
        log.lines()
            .any(|l| l.contains("unprocessed") && l.contains("QUEUED NEVER SUBMITTED"))
    );
    let event = if kind == "acp" {
        "unprocessed"
    } else {
        "delivery_uncertain"
    };
    assert!(
        log.lines()
            .any(|l| l.contains(event) && l.contains("FIRST TIMEOUT MAIL"))
    );
}
#[tokio::test]
async fn acp_timeout_preserves_unsent_mail_and_stops() {
    timeout_boundary("acp").await;
}
#[tokio::test]
async fn codex_timeout_records_uncertain_delivery_without_resend() {
    timeout_boundary("codex").await;
}

#[tokio::test]
async fn acp_load_replay_accepts_only_parent_effort_metadata() {
    lifecycle("acp", "load").await;
}
