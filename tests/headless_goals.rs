//! Fake Codex process + isolated loopback board; no accounts or model calls.
use bbs_core::proto;
use proto::bbs_client::BbsClient;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tonic::Request;

struct Rig {
    home: tempfile::TempDir,
    server: Child,
    runner: Child,
    addr: String,
}

fn req<T>(message: T) -> Request<T> {
    let mut r = Request::new(message);
    for (key, value) in [("bbs-protocol-epoch", "3"), ("bbs-human", "tester")] {
        r.metadata_mut().insert(key, value.parse().unwrap());
    }
    r
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
        let mut client = BbsClient::connect(format!("http://{addr}")).await.unwrap();
        client
            .ensure_human(req(proto::EnsureHumanRequest {
                name: "tester".into(),
            }))
            .await
            .unwrap();
        client
            .ensure_human(req(proto::EnsureHumanRequest {
                name: "observer".into(),
            }))
            .await
            .unwrap();
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_codex_goal.py");
        std::fs::write(home.path().join(".config/bbs/config.toml"), format!(
            "human = \"tester\"\n[headless]\nmin_turn_interval = \"0s\"\n[headless.harness.fake]\ntype = \"codex-app-server\"\nshare_activity = false\ncommand = [\"python3\", {fake:?}, {scenario:?}]\n"
        )).unwrap();
        if scenario.starts_with("resume") {
            let agent = client
                .register(req(proto::RegisterRequest {
                    name: Some("GoalTest".into()),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner();
            let saved = home.path().join(".local/state/bbs/headless/goaltest");
            std::fs::create_dir_all(&saved).unwrap();
            std::fs::write(saved.join("state.json"), serde_json::to_vec(&serde_json::json!({"server":addr,"token":agent.token,"harness":"fake","session_id":"goal-parent","dynamic_tools":true})).unwrap()).unwrap();
        }
        let runner = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["headless", "run", "GoalTest", "--harness", "fake", "--cwd"])
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
        rig.wait("goal-wire.jsonl", |rows| {
            rows.iter().any(|r| r["kind"] == "ready")
        })
        .await;
        rig
    }

    fn rows(&self, path: &str) -> Vec<Value> {
        std::fs::read_to_string(self.home.path().join(path))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }

    async fn wait(&self, path: &str, predicate: impl Fn(&[Value]) -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !predicate(&self.rows(path)) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "missing boundary in {path}: {:?}; fixture stderr: {}",
                self.rows(path),
                std::fs::read_to_string(
                    self.home
                        .path()
                        .join(".local/state/bbs/headless/goaltest/harness.stderr")
                )
                .unwrap_or_default()
            )
        });
    }

    async fn send(&self, body: &str, control: Option<proto::Control>) {
        BbsClient::connect(format!("http://{}", self.addr))
            .await
            .unwrap()
            .send(req(proto::SendRequest {
                to: "GoalTest".into(),
                body: body.into(),
                control: control.map(Into::into),
                reply_to: None,
                not_to_owner: false,
            }))
            .await
            .unwrap();
    }

    fn log(&self) -> Vec<Value> {
        self.rows(".local/state/bbs/headless/goaltest/log.jsonl")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if self.runner.try_wait().unwrap().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.runner.id().to_string()])
                .status();
        }
        let _ = self.runner.wait();
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

async fn chain(scenario: &str, cancel: bool) {
    let rig = Rig::new(scenario).await;
    rig.send("START", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "waiting")
    })
    .await;
    let log = ".local/state/bbs/headless/goaltest/log.jsonl";
    rig.wait(log, |r| r.iter().any(|r| r["event"] == "owner_delivery"))
        .await;
    assert!(
        !rig.log().iter().any(|r| r["event"] == "turn_end"),
        "the goal still owns the turn"
    );
    if cancel {
        rig.send("Cancel", Some(proto::Control::Cancel)).await;
        rig.wait("goal-wire.jsonl", |r| {
            r.iter().any(|r| r["kind"] == "interrupted")
        })
        .await;
    } else {
        rig.send("MAIL_DURING_GOAL", None).await;
        rig.wait("goal-wire.jsonl", |r| {
            r.iter().any(|r| r["kind"] == "steered")
        })
        .await;
    }
    rig.wait(log, |r| r.iter().any(|r| r["event"] == "turn_end"))
        .await;
    if !cancel {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let report = BbsClient::connect(format!("http://{}", rig.addr))
                    .await
                    .unwrap()
                    .side_report(req(proto::SideReportRequest {
                        agent: "GoalTest".into(),
                        generation: None,
                    }))
                    .await
                    .unwrap()
                    .into_inner();
                if report.usage.is_some_and(|u| u.used_tokens == Some(88)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("final continuation usage was not reported");
    }
    assert_eq!(
        rig.rows("goal-wire.jsonl")
            .iter()
            .filter(|r| r["kind"] == "prompt")
            .count(),
        1
    );
    rig.send("AFTER_GOAL", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "fresh-finished")
    })
    .await;
    rig.wait(log, |r| {
        r.iter().filter(|r| r["event"] == "owner_delivery").count() == 2
    })
    .await;
    let rows = rig.log();
    assert!(!rows.iter().any(|r| r["event"] == "turn_error"));
    assert!(!rows.iter().any(|r| r["text"] == "FORBIDDEN OUTPUT"));
    assert!(
        rows.iter()
            .any(|r| r["text"] == "Continuation output" && r["source"]["turn"] == "goal-2")
    );
    assert!(rows.iter().any(|r| r["event"] == "turn_end"
        && r["stop_reason"] == if cancel { "interrupted" } else { "completed" }));
    let received = BbsClient::connect(format!("http://{}", rig.addr))
        .await
        .unwrap()
        .read(req(proto::ReadRequest {
            limit: Some(100),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let bodies: Vec<_> = received.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["Continuation output", "Fresh prompt delivered"]);
    let mut request = req(proto::ReadRequest {
        limit: Some(100),
        ..Default::default()
    });
    request
        .metadata_mut()
        .insert("bbs-human", "observer".parse().unwrap());
    let received = BbsClient::connect(format!("http://{}", rig.addr))
        .await
        .unwrap()
        .read(request)
        .await
        .unwrap()
        .into_inner();
    let bodies: Vec<_> = received.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["from-goal-1", "from-goal-2"]);
}

#[tokio::test]
async fn goal_tools_activity_and_mail_steering_after_start_ack() {
    chain("after-ack", false).await;
}

#[tokio::test]
async fn goal_callbacks_before_start_ack_and_completion_before_last_turn_end() {
    chain("before-ack", false).await;
}

#[tokio::test]
async fn owner_cancel_targets_the_native_continuation() {
    chain("cancel", true).await;
}

#[tokio::test]
async fn resumed_active_goal_is_read_before_the_prompt() {
    chain("resume", false).await;
}

#[tokio::test]
async fn cancelling_between_turns_pauses_the_goal_and_releases_the_prompt() {
    let rig = Rig::new("gap-cancel").await;
    rig.send("START", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "between")
    })
    .await;
    rig.send("Cancel", Some(proto::Control::Cancel)).await;
    rig.wait(".local/state/bbs/headless/goaltest/log.jsonl", |r| {
        r.iter()
            .any(|r| r["event"] == "turn_end" && r["stop_reason"] == "interrupted")
    })
    .await;
    let wire = rig.rows("goal-wire.jsonl");
    assert_eq!(wire.iter().filter(|r| r["kind"] == "paused").count(), 1);
    assert!(!wire.iter().any(|r| r["kind"] == "interrupted"));
    rig.send("AFTER_GOAL", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "fresh-finished")
    })
    .await;
}

#[tokio::test]
async fn process_exit_between_goal_turns_does_not_leave_the_prompt_held() {
    let rig = Rig::new("gap-exit").await;
    rig.send("START", None).await;
    rig.wait(".local/state/bbs/headless/goaltest/log.jsonl", |r| {
        r.iter().any(|r| r["event"] == "stop")
    })
    .await;
    assert!(
        rig.log()
            .iter()
            .any(|r| r["event"] == "stop"
                && r["error"].as_str().is_some_and(|s| s.contains("exited")))
    );
}

#[tokio::test]
async fn resume_does_not_use_a_foreign_goal_or_overwrite_a_newer_notification() {
    for scenario in ["resume-foreign", "resume-race"] {
        let rig = Rig::new(scenario).await;
        rig.send("START", None).await;
        rig.wait(".local/state/bbs/headless/goaltest/log.jsonl", |r| {
            r.iter().any(|r| r["event"] == "turn_end")
        })
        .await;
        assert!(
            rig.rows("goal-wire.jsonl")
                .iter()
                .any(|r| r["kind"] == "goal-read")
        );
    }
}

#[tokio::test]
async fn failed_goal_pause_still_interrupts_and_reports_the_pause_failure() {
    for scenario in ["pause-denied", "pause-unconfirmed", "pause-timeout"] {
        let rig = Rig::new(scenario).await;
        rig.send("START", None).await;
        rig.wait("goal-wire.jsonl", |r| {
            r.iter().any(|r| r["kind"] == "waiting")
        })
        .await;
        rig.send("Cancel", Some(proto::Control::Cancel)).await;
        rig.wait("goal-wire.jsonl", |r| {
            r.iter().any(|r| r["kind"] == "interrupted")
        })
        .await;
        let wire = rig.rows("goal-wire.jsonl");
        let paused = wire
            .iter()
            .position(|r| r["kind"] == "pause-failed")
            .unwrap();
        let interrupted = wire
            .iter()
            .position(|r| r["kind"] == "interrupted")
            .unwrap();
        assert!(paused < interrupted);
        rig.wait(".local/state/bbs/headless/goaltest/log.jsonl", |r| {
            r.iter().any(|r| {
                r["event"] == "error"
                    && r["message"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("cancel:"))
            })
        })
        .await;
    }
}

#[tokio::test]
async fn goal_completion_before_start_ack_preserves_continuation_output() {
    let rig = Rig::new("closed-before-ack").await;
    rig.send("START", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "early-finished")
    })
    .await;
    rig.wait(".local/state/bbs/headless/goaltest/log.jsonl", |r| {
        r.iter().any(|r| r["event"] == "owner_delivery")
    })
    .await;
    let received = BbsClient::connect(format!("http://{}", rig.addr))
        .await
        .unwrap()
        .read(req(proto::ReadRequest {
            limit: Some(100),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        received
            .messages
            .iter()
            .map(|m| m.body.as_str())
            .collect::<Vec<_>>(),
        ["Early continuation output"]
    );
}

#[tokio::test]
async fn a_runner_goal_is_refused_while_a_native_goal_runs() {
    let rig = Rig::new("plain").await;
    rig.send("START", None).await;
    rig.wait("goal-wire.jsonl", |r| {
        r.iter().any(|r| r["kind"] == "waiting")
    })
    .await;
    rig.send("Ship it", Some(proto::Control::GoalSet)).await;
    let log = ".local/state/bbs/headless/goaltest/log.jsonl";
    rig.wait(log, |r| {
        r.iter().any(|r| {
            r["event"] == "warning"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("native goal running"))
        })
    })
    .await;
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(
            rig.home
                .path()
                .join(".local/state/bbs/headless/goaltest/state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(state["goal"].is_null(), "{state}");
    // A fresh thread is started with the goal tool.
    assert_eq!(state["goal_tool"], true);
}
