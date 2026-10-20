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
    runner: Child,
    addr: String,
}
impl Rig {
    async fn new(kind: &str, scenario: &str) -> Self {
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
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_interactions.py");
        let harness = if kind == "acp" {
            "acp"
        } else {
            "codex-app-server"
        };
        std::fs::write(home.path().join(".config/bbs/config.toml"),format!("human = \"tester\"\n[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.fake]\ntype = {harness:?}\nshare_activity = false\ncommand = [\"python3\",{fake:?},{kind:?},{scenario:?}]\n")).unwrap();
        let mut client = BbsClient::connect(format!("http://{addr}")).await.unwrap();
        client
            .ensure_human(req(proto::EnsureHumanRequest {
                name: "tester".into(),
            }))
            .await
            .unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["headless", "run", "InputTest", "--harness", "fake", "--cwd"])
            .arg(home.path())
            .env("HOME", home.path())
            .env("BBS_SERVER", &addr)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let rig = Self {
            home,
            server,
            runner,
            addr,
        };
        rig.wait_wire(|w| w.iter().any(|v| v["kind"] == "ready"))
            .await;
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
                to: "InputTest".into(),
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
                agent: "InputTest".into(),
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
                let r = self.report().await;
                if p(&r) {
                    return r;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("side report did not arrive")
    }
    fn wire(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home.path().join("interaction-wire.jsonl"))
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
    async fn answer(
        &self,
        id: i64,
        action: proto::AnswerAction,
        answers: Vec<proto::InputAnswer>,
    ) -> Result<(), tonic::Status> {
        self.client()
            .await
            .answer_owner_input(req(proto::AnswerOwnerInputRequest {
                request_id: id,
                action: action.into(),
                answers,
            }))
            .await
            .map(|_| ())
    }
    fn stop(&mut self) {
        if self.runner.try_wait().unwrap().is_none() {
            Command::new("kill")
                .args(["-TERM", &self.runner.id().to_string()])
                .status()
                .unwrap();
            assert!(self.runner.wait().unwrap().success());
        }
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

async fn answered(kind: &str) {
    let r = Rig::new(kind, "double").await;
    assert!(r.report().await.usage.is_none());
    r.send("ASK", None).await;
    let form = r
        .wait_report(|r| {
            r.pending_input
                .as_ref()
                .is_some_and(|i| i.status() == proto::InputStatus::Pending)
        })
        .await
        .pending_input
        .unwrap();
    if kind == "acp" {
        assert_eq!(form.message.as_deref(), Some("Which deployment strategy?"));
        assert_eq!(form.questions.len(), 2);
        assert_eq!(form.questions[0].header, "Strategy");
    }
    r.wait_wire(|w| {
        ["foreign", "duplicate"].iter().all(|id| {
            w.iter()
                .any(|v| v["id"] == *id && v["response"]["error"].is_object())
        })
    })
    .await;
    r.answer(
        form.request_id,
        proto::AnswerAction::Submit,
        vec![proto::InputAnswer {
            question_id: "choice".into(),
            value: if kind == "acp" { "safe" } else { "custom text" }.into(),
        }],
    )
    .await
    .unwrap();
    r.wait_wire(|w| w.iter().any(|v| v["kind"] == "finished"))
        .await;
    let reply = r
        .wire()
        .into_iter()
        .find(|v| v["id"] == "question")
        .unwrap();
    if kind == "acp" {
        assert_eq!(
            reply["response"]["result"],
            serde_json::json!({"action":"accept","content":{"choice":"safe"}})
        );
    } else {
        assert_eq!(
            reply["response"]["result"],
            serde_json::json!({"answers":{"choice":{"answers":["custom text"]}}})
        );
    }
    let report = r
        .wait_report(|r| r.usage.as_ref().is_some_and(|u| u.used_tokens == Some(80)))
        .await;
    assert_eq!(report.usage.unwrap().limit_tokens, Some(100));
    assert!(
        r.answer(form.request_id, proto::AnswerAction::Cancel, vec![])
            .await
            .is_err()
    );
    assert_eq!(
        r.wire().iter().filter(|v| v["kind"] == "prompt").count(),
        1,
        "answer must never become ordinary prompt mail"
    );
}
#[tokio::test]
async fn codex_native_answer_and_latest_completed_usage() {
    answered("codex").await;
}
#[tokio::test]
async fn acp_native_flat_form_answer_and_context() {
    answered("acp").await;
}

#[tokio::test]
async fn resolved_before_start_ack_never_publishes_or_answers() {
    let r = Rig::new("codex", "resolved").await;
    r.send("ASK", None).await;
    r.wait_report(|r| r.usage.as_ref().is_some_and(|u| u.used_tokens == Some(80)))
        .await;
    assert!(r.report().await.pending_input.is_none());
    assert!(!r.wire().iter().any(|v| v["id"] == "question"));
}
#[tokio::test]
async fn unsupported_acp_form_rejects_without_publishing() {
    let r = Rig::new("acp", "unsupported").await;
    r.send("ASK", None).await;
    r.wait_wire(|w| {
        w.iter()
            .any(|v| v["id"] == "question" && v["response"]["error"].is_object())
    })
    .await;
    r.wait_report(|r| r.usage.as_ref().is_some_and(|u| u.used_tokens == Some(80)))
        .await;
    assert!(r.report().await.pending_input.is_none());
}
#[tokio::test]
async fn cancel_and_shutdown_close_published_input_without_panicking() {
    for kind in ["codex", "acp"] {
        let mut r = Rig::new(kind, "normal").await;
        r.send("ASK", None).await;
        let form = r
            .wait_report(|r| r.pending_input.is_some())
            .await
            .pending_input
            .unwrap();
        r.send("cancel", Some(proto::Control::Cancel)).await;
        r.wait_report(|r| {
            r.pending_input
                .as_ref()
                .is_some_and(|i| i.status() == proto::InputStatus::Cancelled)
        })
        .await;
        assert!(
            r.answer(form.request_id, proto::AnswerAction::Submit, vec![])
                .await
                .is_err()
        );
        r.wait_wire(|w| {
            w.iter()
                .any(|v| v["id"] == "question" && v["response"]["result"].is_object())
        })
        .await;
        r.send("ASK AGAIN", None).await;
        r.wait_report(|r| {
            r.pending_input.as_ref().is_some_and(|i| {
                i.request_id != form.request_id && i.status() == proto::InputStatus::Pending
            })
        })
        .await;
        r.stop();
        assert_eq!(
            r.report().await.pending_input.unwrap().status(),
            proto::InputStatus::Cancelled
        );
    }
}

#[tokio::test]
async fn text_before_native_question_is_delivered_before_any_human_answer() {
    const BEFORE: &str = "  Before the question.\n\n```text\n  exact bytes\n```\n";
    for kind in ["codex", "acp"] {
        let r = Rig::new(kind, "text-before-question").await;
        r.send("ASK", None).await;
        let form = r
            .wait_report(|r| {
                r.pending_input
                    .as_ref()
                    .is_some_and(|i| i.status() == proto::InputStatus::Pending)
            })
            .await
            .pending_input
            .unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mail = r
                    .client()
                    .await
                    .read(req(proto::ReadRequest::default()))
                    .await
                    .unwrap()
                    .into_inner()
                    .messages;
                if let Some(m) = mail.into_iter().find(|m| m.body == BEFORE) {
                    return m;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("text preceding the native question must not wait for the answer");
        assert_eq!(delivered.body, BEFORE);
        assert_eq!(delivered.auto_reply, Some(true));
        assert_eq!(
            r.report().await.pending_input.unwrap().status(),
            proto::InputStatus::Pending
        );
        assert!(
            !r.wire().iter().any(|v| v["id"] == "question"),
            "no provider answer yet"
        );
        r.answer(form.request_id, proto::AnswerAction::Decline, vec![])
            .await
            .unwrap();
        r.wait_wire(|w| w.iter().any(|v| v["kind"] == "finished"))
            .await;
        assert_eq!(r.wire().iter().filter(|v| v["kind"] == "prompt").count(), 1);
    }
}
