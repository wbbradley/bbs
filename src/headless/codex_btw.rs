//! One ephemeral Codex side conversation. Nothing here emits parent events.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio::time::{Duration, Instant, timeout, timeout_at};

use super::{
    mcp::Tools,
    rpc::{Incoming, Rpc},
};

pub const MAX_QUESTION: usize = 4 * 1024;
pub const MAX_ANSWER: usize = 64 * 1024;
pub const LIFETIME: Duration = Duration::from_secs(120);
const CLEANUP: Duration = Duration::from_secs(5);
const SETUP: Duration = Duration::from_secs(10);
const PREFIX: &str = "You are answering a side question alongside the main agent. Tools, files, and services are shared with the main agent. Avoid interfering with shared data or systems. Answer this side question only.";

#[derive(Debug, PartialEq)]
pub enum Status {
    Completed,
    Interrupted,
    Failed,
    Unsupported,
}
#[derive(Debug)]
pub struct Outcome {
    pub status: Status,
    pub answer: String,
    pub detail: String,
    pub disable: bool,
}

struct Child {
    thread: String,
    turn: Option<String>,
    active: bool,
    starting: bool,
    completed: bool,
    cancelled: bool,
    text: String,
    seen: HashSet<String>,
    done: HashSet<String>,
    queued: Vec<Incoming>,
    queued_bytes: usize,
    finish: Option<oneshot::Sender<Result<()>>>,
    tools_cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub struct SideChannel {
    child: Mutex<Option<Child>>,
    gate: tokio::sync::Mutex<()>,
}
impl SideChannel {
    pub fn text(&self) -> String {
        self.child
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.text.clone())
            .unwrap_or_default()
    }
    pub fn exited(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            finish(c, Err(anyhow!("provider exited")));
        }
    }

    /// Called before parent dispatch. Known child events never reach that path.
    pub fn handle(
        self: &Arc<Self>,
        rpc: &Rpc,
        msg: Incoming,
        tools: &Arc<tokio::sync::Mutex<Tools>>,
    ) -> Option<Incoming> {
        let params = match &msg {
            Incoming::Request { params, .. } | Incoming::Notification { params, .. } => params,
            Incoming::Exited => return Some(msg),
        };
        let mut state = self.child.lock().unwrap();
        let Some(c) = state
            .as_mut()
            .filter(|c| params["threadId"].as_str() == Some(c.thread.as_str()))
        else {
            return Some(msg);
        };
        if !c.active {
            if let Incoming::Request { id, .. } = msg {
                rpc.respond_later(id, Err("side question is closed".into()));
            }
            return None;
        }
        let scoped = match &msg {
            Incoming::Request { .. } => true,
            Incoming::Notification { method, .. } => {
                method.starts_with("item/") || method.starts_with("turn/")
            }
            _ => false,
        };
        if !scoped {
            return None;
        }
        let incoming = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str());
        if c.starting {
            let size = params.to_string().len();
            if c.queued.len() >= 256 || c.queued_bytes + size > 2 * MAX_ANSWER {
                if let Incoming::Request { id, .. } = msg {
                    rpc.respond_later(id, Err("side event buffer is full".into()));
                }
                finish(c, Err(anyhow!("side event buffer exceeded its bound")));
            } else {
                c.queued_bytes += size;
                c.queued.push(msg);
            }
            return None;
        }
        if incoming.is_none() || incoming != c.turn.as_deref() {
            if let Incoming::Request { id, .. } = msg {
                rpc.respond_later(id, Err("request is not in the active side turn".into()));
            }
            return None;
        }
        match msg {
            Incoming::Notification { method, params } => notification(c, &method, &params),
            Incoming::Request { id, method, params } => {
                if c.cancelled {
                    rpc.respond_later(id, Err("side turn cancelled".into()));
                    return None;
                }
                let mut cancelled = c.tools_cancel.subscribe();
                let thread = c.thread.clone();
                let turn = c.turn.clone();
                drop(state);
                if method == "item/tool/requestUserInput" {
                    rpc.respond_later(id, Ok(json!({"answers":{}})));
                    return None;
                }
                if method != "item/tool/call" {
                    // Same supported approval decisions as the parent's never policy,
                    // without recording a parent Permission event.
                    let reply = match method.as_str() {
                        "item/commandExecution/requestApproval"
                        | "item/fileChange/requestApproval" => Ok(json!({"decision":"accept"})),
                        _ => Err(format!("side questions do not support {method}")),
                    };
                    rpc.respond_later(id, reply);
                    return None;
                }
                let rpc = rpc.clone();
                let tools = tools.clone();
                let channel = self.clone();
                tokio::spawn(async move {
                    let work = async {
                        let mut tools = tools.lock().await;
                        ensure!(
                            channel.matches(&thread, turn.as_deref()),
                            "side turn is closed"
                        );
                        tools
                            .call(
                                params["tool"].as_str().unwrap_or_default(),
                                &params["arguments"],
                            )
                            .await
                    };
                    let result = tokio::select! {biased; _=cancelled.changed()=>Err(anyhow!("side turn cancelled")), r=work=>r};
                    let result = if channel.matches(&thread, turn.as_deref()) {
                        result
                    } else {
                        Err(anyhow!("side turn closed before tool response"))
                    };
                    let success = result.is_ok();
                    let text = match result {
                        Ok(text) => text,
                        Err(e) => format!("{e:#}"),
                    };
                    let reply = json!({"contentItems":[{"type":"inputText","text":text}],"success":success});
                    let _ = timeout(CLEANUP, rpc.respond(id, Ok(reply))).await;
                });
                return None;
            }
            Incoming::Exited => unreachable!(),
        }
        None
    }

    fn matches(&self, thread: &str, turn: Option<&str>) -> bool {
        self.child.lock().unwrap().as_ref().is_some_and(|c| {
            c.active && !c.cancelled && c.thread == thread && c.turn.as_deref() == turn
        })
    }

    pub async fn run(
        self: &Arc<Self>,
        rpc: &Rpc,
        parent: &str,
        question: &str,
        mut cancel: watch::Receiver<bool>,
        tools: &Arc<tokio::sync::Mutex<Tools>>,
    ) -> Outcome {
        let Ok(_guard) = self.gate.try_lock() else {
            return Outcome {
                status: Status::Failed,
                answer: String::new(),
                detail: "another side question is active".into(),
                disable: false,
            };
        };
        if question.trim().is_empty() || question.len() > MAX_QUESTION {
            return Outcome {
                status: Status::Failed,
                answer: String::new(),
                detail: "side question must be 1–4096 bytes".into(),
                disable: false,
            };
        }
        *self.child.lock().unwrap() = None;
        let deadline = Instant::now() + LIFETIME;
        let work_deadline = deadline - CLEANUP;
        let mut unsupported = false;
        let mut ready = false;
        let mut submitted = false;
        let mut interrupted = false;
        let work = async {
            // No lastTurnId: the native snapshot includes the active parent's context.
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled before fork");
            }
            let fork_reply = observed_call(
                rpc,
                "thread/fork",
                json!({"threadId":parent,"ephemeral":true,"excludeTurns":true,"deferGoalContinuation":true}),
                Late::Fork(parent.into()),
            );
            let fork = timeout_at(work_deadline.min(Instant::now() + SETUP), fork_reply).await??;
            let fork = fork.map_err(|e| {
                unsupported = matches!(e.code, -32601 | -32602);
                anyhow!("thread/fork: {}", e.message)
            })?;
            let thread = fork["thread"]["id"]
                .as_str()
                .context("fork returned no child id")?
                .to_string();
            ensure!(
                thread != parent && fork["thread"]["forkedFromId"] == parent,
                "fork did not identify a child of the parent"
            );
            // Retain a safe child id for cleanup even if its metadata is rejected.
            let (stop, _) = watch::channel(false);
            *self.child.lock().unwrap() = Some(Child {
                thread: thread.clone(),
                turn: None,
                active: true,
                starting: false,
                completed: false,
                cancelled: false,
                text: String::new(),
                seen: HashSet::new(),
                done: HashSet::new(),
                queued: Vec::new(),
                queued_bytes: 0,
                finish: None,
                tools_cancel: stop,
            });
            ensure!(
                fork["thread"]["ephemeral"] == true && fork["thread"]["forkedFromId"] == parent,
                "provider did not confirm an isolated ephemeral fork"
            );
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled before prompt");
            }
            // A fork can inherit a Goal. Prevent an automatic continuation after our turn.
            timeout_at(
                work_deadline.min(Instant::now() + SETUP),
                rpc.call("thread/goal/clear", json!({"threadId":thread})),
            )
            .await?
            .map_err(|e| {
                unsupported = matches!(e.code, -32601 | -32602);
                anyhow!("child goal clear: {}", e.message)
            })?;
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled before prompt");
            }
            ready = true;
            let (done, result) = oneshot::channel();
            {
                let mut state = self.child.lock().unwrap();
                let c = state.as_mut().unwrap();
                c.starting = true;
                c.finish = Some(done);
            }
            submitted = true;
            let mut ack_reply = observed_call(
                rpc,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":format!("{PREFIX}\n\n{question}")}]}),
                Late::Turn(thread.clone()),
            );
            let ack=tokio::select! {biased;
                _=cancel.changed()=>{
                    interrupted=true;
                    { let mut state=self.child.lock().unwrap();let c=state.as_mut().unwrap();c.cancelled=true;let _=c.tools_cancel.send(true); }
                    // Keep the exact reply receiver during a short cancellation grace.
                    // If it arrives later, the observer performs only child cleanup.
                    timeout_at(work_deadline.min(Instant::now()+Duration::from_secs(3)),&mut ack_reply).await??
                },
                r=timeout_at(work_deadline,&mut ack_reply)=>r??,
            }.map_err(|e|anyhow!("child turn/start: {}",e.message))?;
            let turn = ack["turn"]["id"]
                .as_str()
                .context("child turn/start returned no id")?
                .to_string();
            let queued = {
                let mut state = self.child.lock().unwrap();
                let c = state.as_mut().unwrap();
                c.turn = Some(turn);
                c.starting = false;
                std::mem::take(&mut c.queued)
            };
            for msg in queued {
                self.handle(rpc, msg, tools);
            }
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled");
            }
            tokio::select! {biased;
                _=cancel.changed()=>{interrupted=true;Err(anyhow!("side question cancelled"))},
                _=tokio::time::sleep_until(work_deadline)=>{interrupted=true;Err(anyhow!("side question exceeded its lifetime"))},
                r=result=>r.context("side completion channel closed")?,
            }
        };
        let result = work.await;
        let (answer, thread, turn, needs_interrupt) = {
            let mut state = self.child.lock().unwrap();
            if let Some(c) = state.as_mut() {
                let complete = c.completed;
                c.active = false;
                let _ = c.tools_cancel.send(true);
                (
                    c.text.clone(),
                    Some(c.thread.clone()),
                    c.turn.clone(),
                    !complete && submitted,
                )
            } else {
                (String::new(), None, None, false)
            }
        };
        // Keep all cleanup inside the total lifetime. Never interrupt the parent.
        let mut cleanup_ok = true;
        if let Some(thread) = thread {
            if needs_interrupt {
                if let Some(turn) = turn {
                    cleanup_ok = matches!(
                        timeout_at(
                            deadline.min(Instant::now() + Duration::from_secs(2)),
                            rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
                        )
                        .await,
                        Ok(Ok(_))
                    );
                } else {
                    cleanup_ok = false;
                }
            }
            cleanup_ok &= matches!(
                timeout_at(
                    deadline,
                    rpc.call("thread/unsubscribe", json!({"threadId":thread}))
                )
                .await,
                Ok(Ok(_))
            );
        }
        let status = if unsupported {
            Status::Unsupported
        } else if interrupted {
            Status::Interrupted
        } else if result.is_ok() {
            Status::Completed
        } else {
            Status::Failed
        };
        let setup_failed = !ready && !interrupted && result.is_err();
        let mut detail = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
        if !cleanup_ok {
            detail.push_str("; child cleanup unconfirmed; shared tools may already have run");
        }
        Outcome {
            status,
            answer,
            detail,
            disable: unsupported || !cleanup_ok || setup_failed,
        }
    }
}

/// An abandoned reply still has one owner. It may only clean its returned child,
/// never start another turn or touch a parent identity. At most one uncertain
/// setup is allowed before the runner disables side requests.
enum Late {
    Fork(String),
    Turn(String),
}
fn observed_call(
    rpc: &Rpc,
    method: &'static str,
    params: Value,
    late: Late,
) -> oneshot::Receiver<std::result::Result<Value, super::rpc::RpcError>> {
    let (send, recv) = oneshot::channel();
    let rpc = rpc.clone();
    tokio::spawn(async move {
        let result = rpc.call(method, params).await;
        if let Err(Ok(value)) = send.send(result) {
            let deadline = Instant::now() + CLEANUP;
            match late {
                Late::Fork(parent) => {
                    if let Some(thread) = value["thread"]["id"]
                        .as_str()
                        .filter(|id| *id != parent && value["thread"]["forkedFromId"] == parent)
                    {
                        let _ = timeout_at(
                            deadline,
                            rpc.call("thread/goal/clear", json!({"threadId":thread})),
                        )
                        .await;
                        let _ = timeout_at(
                            deadline,
                            rpc.call("thread/unsubscribe", json!({"threadId":thread})),
                        )
                        .await;
                    }
                }
                Late::Turn(thread) => {
                    if let Some(turn) = value["turn"]["id"].as_str() {
                        let _ = timeout_at(
                            deadline,
                            rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":turn})),
                        )
                        .await;
                    }
                    let _ = timeout_at(
                        deadline,
                        rpc.call("thread/unsubscribe", json!({"threadId":thread})),
                    )
                    .await;
                }
            }
        }
    });
    recv
}

fn finish(c: &mut Child, result: Result<()>) {
    c.active = false;
    let _ = c.tools_cancel.send(true);
    if let Some(done) = c.finish.take() {
        let _ = done.send(result);
    }
}
fn append(c: &mut Child, text: &str) {
    let available = MAX_ANSWER.saturating_sub(c.text.len());
    if text.len() > available {
        let mut end = available;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        c.text.push_str(&text[..end]);
        finish(c, Err(anyhow!("side answer exceeded 64 KiB")));
    } else {
        c.text.push_str(text);
    }
}
fn notification(c: &mut Child, method: &str, p: &Value) {
    match method {
        "item/agentMessage/delta" => {
            if let (Some(id), Some(text)) = (p["itemId"].as_str(), p["delta"].as_str()) {
                if !text.is_empty() && !c.done.contains(id) {
                    c.seen.insert(id.into());
                    append(c, text);
                }
            }
        }
        "item/completed" if p["item"]["type"] == "agentMessage" => {
            if let Some(id) = p["item"]["id"].as_str() {
                if c.done.insert(id.into()) && !c.seen.contains(id) {
                    if let Some(text) = p["item"]["text"].as_str() {
                        append(c, text);
                    }
                }
            }
        }
        "turn/completed" => {
            let status = p["turn"]["status"].as_str().unwrap_or("unknown");
            c.completed = true;
            finish(
                c,
                if status == "completed" {
                    Ok(())
                } else {
                    Err(anyhow!("side turn {status}"))
                },
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::{
        acp::McpServer,
        harness::{BbsTools, Event, Harness},
        rpc::Process,
        settings::{HarnessConfig, HarnessType},
    };
    struct Rig {
        dir: tempfile::TempDir,
        harness: Harness,
        process: Process,
        events: mpsc::UnboundedReceiver<Event>,
    }
    use tokio::sync::mpsc;
    impl Rig {
        async fn new(scenario: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config = HarnessConfig {
                kind: HarnessType::CodexAppServer,
                command: vec![
                    "python3".into(),
                    format!("{}/tests/fake_btw.py", env!("CARGO_MANIFEST_DIR")),
                    scenario.into(),
                ],
                ..Default::default()
            };
            let (harness, process, events, _) = Harness::start(
                &config,
                dir.path(),
                &dir.path().join("stderr"),
                BbsTools {
                    mcp: McpServer {
                        name: "bbs".into(),
                        command: "unused".into(),
                        args: vec![],
                        env: vec![],
                    },
                    tools: Tools::new(dir.path(), "synthetic-unused".into()),
                    input_enabled: true,
                },
                None,
            )
            .await
            .unwrap();
            Self {
                dir,
                harness,
                process,
                events,
            }
        }
        fn wire(&self) -> Vec<Value> {
            std::fs::read_to_string(self.dir.path().join("btw-wire.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        }
        async fn wait(&self, p: impl Fn(&[Value]) -> bool) {
            timeout(Duration::from_secs(5), async {
                while !p(&self.wire()) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        async fn parent(&mut self) -> tokio::task::JoinHandle<Result<String>> {
            let h = self.harness.clone();
            let task = tokio::spawn(async move { h.prompt("MAIN CONTEXT", None).await });
            self.wait(|w| w.iter().any(|v| v["kind"] == "parent_active"))
                .await;
            task
        }
        fn side(&self, cancel: watch::Receiver<bool>) -> tokio::task::JoinHandle<Outcome> {
            let h = self.harness.clone();
            tokio::spawn(async move { h.side_question("SIDE QUESTION", cancel).await })
        }
        async fn finish(&mut self, parent: tokio::task::JoinHandle<Result<String>>) {
            self.harness.cancel().await.unwrap();
            timeout(Duration::from_secs(5), parent)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            self.process.stop().await;
        }
    }
    #[tokio::test]
    async fn parent_continues_and_child_callbacks_never_reach_parent_events() {
        for scenario in ["normal", "early"] {
            let mut r = Rig::new(scenario).await;
            let parent = r.parent().await;
            let (_cancel, rx) = watch::channel(false);
            let result = timeout(Duration::from_secs(5), r.side(rx))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.status, Status::Completed);
            assert_eq!(result.answer, "side prefix\nside final\n");
            assert!(!result.disable);
            assert!(!parent.is_finished());
            r.harness.steer("ordinary parent mail").await.unwrap();
            r.wait(|w| {
                w.iter()
                    .any(|v| v["kind"] == "rpc" && v["method"] == "turn/steer")
            })
            .await;
            while let Ok(e) = r.events.try_recv() {
                match e {
                    Event::Text(_, t) => {
                        assert!(!t.starts_with("side") && !t.contains("STALE SIDE"))
                    }
                    Event::Permission(_) | Event::OwnerInput(_) => {
                        panic!("child callback entered parent events")
                    }
                    Event::ContextChanged(Some(u)) => assert_ne!(u.used_tokens, Some(99999)),
                    _ => {}
                }
            }
            let wire = r.wire();
            let native = wire.iter().find(|v| v["id"] == "native").unwrap();
            assert_eq!(native["response"]["result"], json!({"answers":{}}));
            assert!(
                wire.iter().find(|v| v["id"] == "foreign").unwrap()["response"]["error"]
                    .is_object()
            );
            assert!(
                wire.iter()
                    .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "parent")
                    .all(|v| !v["params"].to_string().contains("SIDE QUESTION"))
            );
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn unsupported_prerequisites_never_submit_child_prompt() {
        for scenario in ["no-fork", "bad-fork", "no-goal"] {
            let mut r = Rig::new(scenario).await;
            let parent = r.parent().await;
            let (_cancel, rx) = watch::channel(false);
            let result = r.side(rx).await.unwrap();
            assert!(matches!(
                result.status,
                Status::Unsupported | Status::Failed
            ));
            assert!(result.disable);
            assert!(!parent.is_finished());
            assert!(
                !r.wire()
                    .iter()
                    .any(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
            );
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn cancellation_and_output_bound_interrupt_only_the_child() {
        for scenario in ["hold", "overflow"] {
            let mut r = Rig::new(scenario).await;
            let parent = r.parent().await;
            let (cancel, rx) = watch::channel(false);
            let task = r.side(rx);
            if scenario == "hold" {
                r.wait(|w| w.iter().any(|v| v["kind"] == "side_active"))
                    .await;
                cancel.send(true).unwrap();
            }
            let result = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                result.status,
                Status::Interrupted | Status::Failed
            ));
            assert!(result.answer.starts_with("side prefix\n"));
            assert!(result.answer.len() <= MAX_ANSWER);
            let interrupts: Vec<_> = r
                .wire()
                .into_iter()
                .filter(|v| v["method"] == "turn/interrupt")
                .collect();
            assert_eq!(interrupts.len(), 1);
            assert_eq!(interrupts[0]["params"]["threadId"], "child");
            assert!(!parent.is_finished());
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn late_fork_and_start_acks_are_owned_by_child_cleanup() {
        for scenario in ["late-fork", "late-start"] {
            let mut r = Rig::new(scenario).await;
            let parent = r.parent().await;
            let (cancel, rx) = watch::channel(false);
            let task = r.side(rx);
            let marker = if scenario == "late-fork" {
                "fork_pending"
            } else {
                "start_pending"
            };
            r.wait(|w| w.iter().any(|v| v["kind"] == marker)).await;
            if scenario == "late-start" {
                cancel.send(true).unwrap();
            }
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(20)).await;
            // Resume real I/O time before checking the bounded caller's outcome.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::resume();
            let result = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert!(result.disable);
            std::fs::write(
                r.dir.path().join(if scenario == "late-fork" {
                    "release-fork"
                } else {
                    "release-start"
                }),
                "",
            )
            .unwrap();
            r.wait(|w| {
                w.iter().any(|v| {
                    v["method"]
                        == if scenario == "late-fork" {
                            "thread/goal/clear"
                        } else {
                            "turn/interrupt"
                        }
                })
            })
            .await;
            assert!(!parent.is_finished());
            assert!(!r.wire().iter().any(|v|v["method"]=="turn/interrupt"&&v["params"]["threadId"]=="parent"));
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn queued_and_waiting_child_tools_cannot_outlive_cancellation_or_completion() {
        for scenario in ["tools-cancel", "tools-complete", "late-start-tools"] {
            let mut r = Rig::new(scenario).await;
            let parent = r.parent().await;
            let Harness::Codex(c) = &r.harness else {
                unreachable!()
            };
            let locked = c.side_tools_for_test().lock_owned().await;
            let (cancel, rx) = watch::channel(false);
            let task = r.side(rx);
            r.wait(|w| w.iter().any(|v| v["kind"] == "tool_pending"))
                .await;
            if scenario != "tools-complete" {
                cancel.send(true).unwrap();
            }
            if scenario == "late-start-tools" {
                tokio::task::yield_now().await;
                std::fs::write(r.dir.path().join("release-start"), "").unwrap();
            }
            let result = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                result.status,
                Status::Completed | Status::Interrupted
            ));
            // While the tool client is still locked, a cancelled/closed call must
            // settle without acquiring it or touching the board.
            r.wait(|w| w.iter().any(|v| v["id"] == "tool")).await;
            let reply = r.wire().into_iter().find(|v| v["id"] == "tool").unwrap();
            assert!(
                reply["response"]["error"].is_object()
                    || reply["response"]["result"]["success"] == false
            );
            drop(locked);
            assert!(!parent.is_finished());
            r.finish(parent).await;
        }
    }
}
