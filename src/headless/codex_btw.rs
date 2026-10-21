//! One ephemeral Codex side conversation. Nothing here emits parent events.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot, watch};
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    OneShot,
    Fresh,
    Followup,
}

#[derive(Debug, PartialEq)]
pub enum Status {
    Completed,
    Interrupted,
    Failed,
    Unsupported,
    Ended,
}
#[derive(Debug)]
pub struct Outcome {
    pub status: Status,
    pub answer: String,
    pub detail: String,
    pub disable: bool,
    pub retained: bool,
}

struct Child {
    thread: String,
    turn: Option<String>,
    active: bool,
    starting: bool,
    completed: bool,
    completion: Option<String>,
    pending: usize,
    open_items: HashSet<String>,
    items_uncertain: bool,
    interrupt_ack: bool,
    cancelled: bool,
    submitted: bool,
    cleanup_done: bool,
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
    cleanup_gate: tokio::sync::Mutex<()>,
    closing: AtomicBool,
    changed: Notify,
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
    pub fn retained(&self) -> bool {
        self.child.lock().unwrap().as_ref().is_some_and(|c| {
            c.completed
                && c.pending == 0
                && c.open_items.is_empty()
                && self.retained_without_lock(c)
        })
    }
    pub fn exited(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            c.completed = true;
            c.cleanup_done = true;
            finish(c, Err(anyhow!("provider exited")));
            self.changed.notify_waiters();
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
        let Some(c) = state.as_mut().filter(|c| {
            params["threadId"]
                .as_str()
                .or_else(|| params["conversationId"].as_str())
                == Some(c.thread.as_str())
        }) else {
            return Some(msg);
        };
        if !c.active {
            // Provider-owned tools (including MCP) are not host callbacks. Their
            // item completion must also be confirmed before another turn.
            if let Incoming::Notification { method, params } = &msg
                && params["turnId"].as_str() == c.turn.as_deref()
                && matches!(method.as_str(), "item/started" | "item/completed")
            {
                track_item(c, method, params);
                self.changed.notify_waiters();
            }
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
            Incoming::Notification { method, params } => {
                notification(c, &method, &params);
                self.changed.notify_waiters();
            }
            Incoming::Request { id, method, params } => {
                if c.cancelled {
                    rpc.respond_later(id, Err("side turn cancelled".into()));
                    return None;
                }
                c.pending += 1;
                let pending = Pending {
                    channel: self.clone(),
                    thread: c.thread.clone(),
                    turn: c.turn.clone(),
                };
                let mut cancelled = c.tools_cancel.subscribe();
                let thread = c.thread.clone();
                let turn = c.turn.clone();
                drop(state);
                if matches!(
                    method.as_str(),
                    "execCommandApproval" | "applyPatchApproval"
                ) {
                    pending.respond(
                        rpc,
                        id,
                        Err("legacy approvals are unsupported in a side turn".into()),
                    );
                    return None;
                }
                if method == "item/tool/requestUserInput" {
                    pending.respond(rpc, id, Ok(json!({"answers":{}})));
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
                    pending.respond(rpc, id, reply);
                    return None;
                }
                let rpc = rpc.clone();
                let tools = tools.clone();
                let channel = self.clone();
                tokio::spawn(async move {
                    let _pending = pending;
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
        inherit: &Value,
        question: &str,
        mode: Mode,
        mut cancel: watch::Receiver<bool>,
        tools: &Arc<tokio::sync::Mutex<Tools>>,
    ) -> Outcome {
        let Ok(_guard) = self.gate.try_lock() else {
            return Outcome {
                status: Status::Failed,
                answer: String::new(),
                detail: "another side question is active".into(),
                disable: false,
                retained: false,
            };
        };
        if question.trim().is_empty() || question.len() > MAX_QUESTION {
            return Outcome {
                status: Status::Failed,
                answer: String::new(),
                detail: "side question must be 1–4096 bytes".into(),
                disable: false,
                retained: false,
            };
        }
        if mode == Mode::Followup && !self.retained() {
            return Outcome {
                status: Status::Ended,
                answer: String::new(),
                detail: "Side conversation ended; start a fresh /btw.".into(),
                disable: false,
                retained: false,
            };
        }
        if self.closing.load(Ordering::SeqCst) {
            return Outcome {
                status: Status::Failed,
                answer: String::new(),
                detail: "previous side cleanup is unconfirmed".into(),
                disable: true,
                retained: false,
            };
        }
        if mode != Mode::Followup {
            if !self.cleanup(rpc, Instant::now() + CLEANUP).await {
                return Outcome {
                    status: Status::Failed,
                    answer: String::new(),
                    detail: "previous side cleanup is unconfirmed".into(),
                    disable: true,
                    retained: false,
                };
            }
            *self.child.lock().unwrap() = None;
        }
        let mut cleanup_guard = CleanupGuard {
            channel: self.clone(),
            rpc: rpc.clone(),
            armed: true,
        };
        let deadline = Instant::now() + LIFETIME;
        let work_deadline = deadline - CLEANUP;
        let mut unsupported = false;
        let mut ready = false;
        let mut interrupted = false;
        let work = async {
            // No lastTurnId: the native snapshot includes the active parent's context.
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled before fork");
            }
            let thread = if mode == Mode::Followup {
                self.child
                    .lock()
                    .unwrap()
                    .as_ref()
                    .context("side child was lost")?
                    .thread
                    .clone()
            } else {
                let fork_reply = observed_call(
                    rpc,
                    "thread/fork",
                    {
                        // A fork inherits plain-function tools, but not the
                        // session's model, provider or MCP servers: given here.
                        let mut fork =
                            json!({"threadId":parent,"ephemeral":true,"excludeTurns":true});
                        if let (Some(fork), Some(inherit)) =
                            (fork.as_object_mut(), inherit.as_object())
                        {
                            fork.extend(inherit.clone());
                        }
                        fork
                    },
                    Late::Fork(parent.into()),
                );
                let fork =
                    timeout_at(work_deadline.min(Instant::now() + SETUP), fork_reply).await??;
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
                    completion: None,
                    pending: 0,
                    open_items: HashSet::new(),
                    items_uncertain: false,
                    interrupt_ack: false,
                    cancelled: false,
                    submitted: false,
                    cleanup_done: false,
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
                thread
            };
            // An ephemeral thread cannot own a goal, so the child inherits none
            // and none can continue automatically after our turn.
            if *cancel.borrow() {
                interrupted = true;
                bail!("side question cancelled before prompt");
            }
            ready = true;
            let (done, result) = oneshot::channel();
            {
                let mut state = self.child.lock().unwrap();
                let c = state.as_mut().unwrap();
                c.active = true;
                c.starting = true;
                c.submitted = true;
                c.turn = None;
                c.completed = false;
                c.completion = None;
                c.interrupt_ack = false;
                c.cancelled = false;
                c.text.clear();
                c.open_items.clear();
                c.items_uncertain = false;
                c.seen.clear();
                c.done.clear();
                c.queued.clear();
                c.queued_bytes = 0;
                c.tools_cancel = watch::channel(false).0;
                c.finish = Some(done);
            }
            let mut ack_reply = observed_call(
                rpc,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":if mode == Mode::Followup { question.to_owned() } else { format!("{PREFIX}\n\n{question}") }}]}),
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
        let cleanup_deadline = deadline.min(Instant::now() + CLEANUP);
        let settle_deadline = cleanup_deadline.min(Instant::now() + Duration::from_secs(2));
        // An interrupt ACK alone does not prove the turn is finished. Wait for
        // its exact terminal event, then for all child callback writes to settle.
        let mut interrupt_confirmed = !interrupted;
        if interrupted && mode != Mode::OneShot {
            let target = {
                let mut state = self.child.lock().unwrap();
                state.as_mut().map(|c| {
                    c.cancelled = true;
                    let _ = c.tools_cancel.send(true);
                    (c.thread.clone(), c.turn.clone(), c.completed)
                })
            };
            if let Some((thread, Some(turn), completed)) = target {
                if completed {
                    interrupt_confirmed = self.child.lock().unwrap().as_ref().is_some_and(|c| {
                        matches!(c.completion.as_deref(), Some("completed" | "failed"))
                            || c.interrupt_ack
                    });
                } else {
                    interrupt_confirmed = matches!(
                        timeout_at(
                            settle_deadline,
                            rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
                        )
                        .await,
                        Ok(Ok(_))
                    );
                    if interrupt_confirmed
                        && let Some(c) = self.child.lock().unwrap().as_mut().filter(|c| {
                            c.thread == thread && c.turn.as_deref() == Some(turn.as_str())
                        })
                    {
                        c.interrupt_ack = true;
                    }
                }
            }
        }
        let terminal_expected = self.child.lock().unwrap().as_ref().is_some_and(|c| {
            c.completed || (interrupted && interrupt_confirmed && c.turn.is_some())
        });
        let retained = mode != Mode::OneShot
            && terminal_expected
            && interrupt_confirmed
            && self.wait_quiescent(settle_deadline).await;
        let cleanup_ok = retained || self.cleanup(rpc, cleanup_deadline).await;
        cleanup_guard.armed = false;
        let answer = self.text();
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
            retained,
        }
    }
    async fn wait_quiescent(&self, deadline: Instant) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.child.lock().unwrap();
                let Some(c) = state.as_ref() else {
                    return false;
                };
                if c.cleanup_done || (!c.active && !c.completed) {
                    return false;
                }
                if c.items_uncertain {
                    return false;
                }
                if c.completed && c.pending == 0 && c.open_items.is_empty() {
                    return self.retained_without_lock(c);
                }
            }
            if timeout_at(deadline, changed).await.is_err() {
                return false;
            }
        }
    }
    fn retained_without_lock(&self, c: &Child) -> bool {
        !self.closing.load(Ordering::SeqCst)
            && !c.cleanup_done
            && !c.items_uncertain
            && (matches!(c.completion.as_deref(), Some("completed" | "failed"))
                || (c.completion.as_deref() == Some("interrupted") && c.interrupt_ack))
    }
    fn fence(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            c.active = false;
            c.cancelled = true;
            let _ = c.tools_cancel.send(true);
        }
    }
    pub async fn cleanup(&self, rpc: &Rpc, deadline: Instant) -> bool {
        self.fence();
        let Ok(_serial) = timeout_at(deadline, self.cleanup_gate.lock()).await else {
            return false;
        };
        let target = {
            let state = self.child.lock().unwrap();
            state.as_ref().filter(|c| !c.cleanup_done).map(|c| {
                (
                    c.thread.clone(),
                    c.turn.clone(),
                    c.submitted && !c.completed,
                )
            })
        };
        let Some((thread, turn, interrupt)) = target else {
            self.closing.store(false, Ordering::SeqCst);
            return true;
        };
        let mut ok = true;
        if interrupt {
            if let Some(turn) = turn {
                ok = matches!(
                    timeout_at(
                        deadline.min(Instant::now() + Duration::from_secs(2)),
                        rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
                    )
                    .await,
                    Ok(Ok(_))
                );
            } else {
                ok = false;
            }
        }
        ok &= matches!(
            timeout_at(
                deadline,
                rpc.call("thread/unsubscribe", json!({"threadId":thread}))
            )
            .await,
            Ok(Ok(_))
        );
        if ok {
            if let Some(c) = self
                .child
                .lock()
                .unwrap()
                .as_mut()
                .filter(|c| c.thread == thread)
            {
                c.cleanup_done = true;
            }
            self.closing.store(false, Ordering::SeqCst);
        }
        ok
    }
}

struct Pending {
    channel: Arc<SideChannel>,
    thread: String,
    turn: Option<String>,
}
impl Pending {
    fn respond(self, rpc: &Rpc, id: Value, reply: std::result::Result<Value, String>) {
        let rpc = rpc.clone();
        tokio::spawn(async move {
            let _pending = self;
            let _ = timeout(CLEANUP, rpc.respond(id, reply)).await;
        });
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(c) = self
            .channel
            .child
            .lock()
            .unwrap()
            .as_mut()
            .filter(|c| c.thread == self.thread && c.turn == self.turn)
        {
            c.pending = c.pending.saturating_sub(1);
        }
        self.channel.changed.notify_waiters();
    }
}

/// Aborting the driver future still cancels host tools synchronously and
/// gives the child-only cleanup a bounded owner.
struct CleanupGuard {
    channel: Arc<SideChannel>,
    rpc: Rpc,
    armed: bool,
}
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.channel.fence();
            let channel = self.channel.clone();
            let rpc = self.rpc.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    channel.cleanup(&rpc, Instant::now() + CLEANUP).await;
                });
            }
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
fn track_item(c: &mut Child, method: &str, p: &Value) {
    if method == "item/started" {
        if let Some(id) = p["item"]["id"].as_str() {
            c.open_items.insert(id.into());
        } else {
            c.items_uncertain = true;
        }
    } else if method == "item/completed"
        && let Some(id) = p["item"]["id"].as_str()
    {
        c.open_items.remove(id);
    }
}
fn notification(c: &mut Child, method: &str, p: &Value) {
    track_item(c, method, p);
    match method {
        "item/agentMessage/delta" => {
            if let (Some(id), Some(text)) = (p["itemId"].as_str(), p["delta"].as_str())
                && !text.is_empty()
                && !c.done.contains(id)
            {
                c.seen.insert(id.into());
                append(c, text);
            }
        }
        "item/completed" if p["item"]["type"] == "agentMessage" => {
            if let Some(id) = p["item"]["id"].as_str()
                && c.done.insert(id.into())
                && !c.seen.contains(id)
                && let Some(text) = p["item"]["text"].as_str()
            {
                append(c, text);
            }
        }
        "turn/completed" => {
            let status = p["turn"]["status"].as_str().unwrap_or("unknown");
            c.completed = true;
            c.completion = Some(status.into());
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
        harness::{BbsTools, Event, Harness, Resume},
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
            Self::open(scenario, None).await
        }
        /// A parent whose model and provider bbs chose, not Codex's defaults.
        async fn with_model() -> Self {
            Self::open("model", None).await
        }
        /// A parent opened through ACP, resumed with its BBS MCP server.
        async fn adopted() -> Self {
            Self::open(
                "adopted",
                Some(Resume {
                    id: "parent",
                    dynamic_tools: false,
                    strict: true,
                }),
            )
            .await
        }
        async fn open(scenario: &str, resume: Option<Resume<'_>>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config = HarnessConfig {
                kind: HarnessType::CodexAppServer,
                command: vec![
                    "python3".into(),
                    format!("{}/tests/fake_btw.py", env!("CARGO_MANIFEST_DIR")),
                    scenario.into(),
                ],
                model: scenario.contains("model").then(|| "fixture-model".into()),
                provider: scenario
                    .contains("model")
                    .then(|| "fixture-provider".into()),
                ..Default::default()
            };
            let (harness, process, events, _) = Harness::start(
                &config,
                dir.path(),
                &dir.path().join("stderr"),
                BbsTools {
                    mcp: McpServer {
                        name: "bbs".into(),
                        command: "bbs-fixture".into(),
                        args: vec![],
                        env: vec![("BBS_HEADLESS_TOKEN".into(), "fixture".into())],
                    },
                    tools: Tools::new(dir.path(), "synthetic-unused".into()),
                    input_enabled: true,
                },
                resume,
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
        fn turn(
            &self,
            question: &str,
            mode: Mode,
            cancel: watch::Receiver<bool>,
        ) -> tokio::task::JoinHandle<Outcome> {
            let h = self.harness.clone();
            let question = question.to_string();
            tokio::spawn(async move { h.side_turn(&question, mode, cancel).await })
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
            for id in ["legacy-command", "legacy-patch", "ambiguous-legacy"] {
                assert!(
                    wire.iter().find(|v| v["id"] == id).unwrap()["response"]["error"].is_object()
                );
            }
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
            // Only what an ephemeral fork accepts: no deferred goal, and no
            // goal methods on a child that cannot own one.
            let fork = wire.iter().find(|v| v["method"] == "thread/fork").unwrap();
            assert_eq!(
                fork["params"],
                json!({"threadId":"parent","ephemeral":true,"excludeTurns":true})
            );
            assert!(!wire.iter().any(|v| {
                v["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("thread/goal/"))
            }));
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn an_adopted_parent_forks_its_side_child_with_its_mcp_server() {
        let mut r = Rig::adopted().await;
        let parent = r.parent().await;
        let (_cancel, rx) = watch::channel(false);
        let result = timeout(Duration::from_secs(5), r.side(rx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(result.answer, "side prefix\nside final\n");
        assert!(!r.harness.dynamic_tools());
        // The fake checked the resume and fork both carry the MCP server.
        let wire = r.wire();
        assert!(!wire.iter().any(|v| v["method"] == "thread/start"));
        let fork = wire.iter().find(|v| v["method"] == "thread/fork").unwrap();
        assert!(fork["params"]["config"]["mcp_servers"]["bbs"].is_object());
        assert!(!parent.is_finished());
        r.finish(parent).await;
    }
    #[tokio::test]
    async fn a_side_child_runs_on_the_parents_model_and_provider() {
        let mut r = Rig::with_model().await;
        let parent = r.parent().await;
        let (_cancel, rx) = watch::channel(false);
        let result = timeout(Duration::from_secs(5), r.side(rx))
            .await
            .unwrap()
            .unwrap();
        // The fake refuses a fork without them; check they are there.
        assert_eq!(result.status, Status::Completed);
        let fork = r
            .wire()
            .into_iter()
            .find(|v| v["method"] == "thread/fork")
            .unwrap();
        assert_eq!(fork["params"]["model"], "fixture-model");
        assert_eq!(fork["params"]["modelProvider"], "fixture-provider");
        r.finish(parent).await;
    }
    #[tokio::test]
    async fn unsupported_prerequisites_never_submit_child_prompt() {
        for scenario in ["no-fork", "bad-fork"] {
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
                            "thread/unsubscribe"
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
    #[tokio::test]
    async fn lifetime_expiry_never_interrupts_or_replays_the_parent() {
        let mut r = Rig::new("hold").await;
        let parent = r.parent().await;
        let (_cancel, rx) = watch::channel(false);
        let task = r.side(rx);
        r.wait(|w| w.iter().any(|v| v["kind"] == "side_active"))
            .await;
        tokio::time::pause();
        tokio::time::advance(LIFETIME).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::resume();
        let result = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, Status::Interrupted);
        assert!(result.answer.starts_with("side prefix"));
        assert!(!parent.is_finished());
        assert_eq!(
            r.wire()
                .iter()
                .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
                .count(),
            1
        );
        assert!(
            !r.wire()
                .iter()
                .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "parent")
        );
        r.finish(parent).await;
    }
    #[tokio::test]
    async fn aborted_driver_owns_child_cleanup_and_cancels_waiting_tools() {
        let mut r = Rig::new("tools-cancel").await;
        let parent = r.parent().await;
        let Harness::Codex(c) = &r.harness else {
            unreachable!()
        };
        let locked = c.side_tools_for_test().lock_owned().await;
        let (_cancel, rx) = watch::channel(false);
        let task = r.side(rx);
        r.wait(|w| w.iter().any(|v| v["kind"] == "tool_pending"))
            .await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(r.harness.finish_side().await);
        r.wait(|w| w.iter().any(|v| v["id"] == "tool")).await;
        let tool = r.wire().into_iter().find(|v| v["id"] == "tool").unwrap();
        assert!(
            tool["response"]["error"].is_object() || tool["response"]["result"]["success"] == false
        );
        assert!(!parent.is_finished());
        assert!(
            !r.wire()
                .iter()
                .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "parent")
        );
        drop(locked);
        r.finish(parent).await;
    }
    #[tokio::test]
    async fn followups_keep_one_child_and_fence_prior_turn_events() {
        for scenario in [
            "continue",
            "continue-early",
            "continue-model",
            "continue-failed",
            "continue-adopted-tool",
        ] {
            let resume = scenario.contains("adopted").then_some(Resume {
                id: "parent",
                dynamic_tools: false,
                strict: true,
            });
            let mut r = Rig::open(scenario, resume).await;
            let parent = r.parent().await;
            for n in 1..=4 {
                let (_cancel, rx) = watch::channel(false);
                let outcome = timeout(
                    Duration::from_secs(5),
                    r.turn(
                        "SIDE ONLY",
                        if n == 1 { Mode::Fresh } else { Mode::Followup },
                        rx,
                    ),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(outcome.answer, format!("side answer {n}\n"));
                assert!(outcome.retained, "{scenario}: {}", outcome.detail);
                assert!(r.harness.side_retained());
                assert!(!parent.is_finished());
            }
            let wire = r.wire();
            assert_eq!(
                wire.iter().filter(|v| v["method"] == "thread/fork").count(),
                1
            );
            assert!(!wire.iter().any(|v| v["method"] == "thread/unsubscribe"));
            let turns: Vec<_> = wire
                .iter()
                .filter(|v| v["method"] == "turn/start" && v["params"]["threadId"] == "child")
                .collect();
            assert_eq!(turns.len(), 4);
            assert!(
                turns[0]["params"]["input"][0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with(PREFIX)
            );
            for turn in &turns[1..] {
                assert_eq!(turn["params"]["input"][0]["text"], "SIDE ONLY");
            }
            while let Ok(e) = r.events.try_recv() {
                if let Event::Text(_, text) = e {
                    assert!(!text.contains("side answer") && !text.contains("SIDE ONLY"));
                }
            }
            assert!(r.harness.finish_side().await);
            let before = r.wire().len();
            let (_cancel, rx) = watch::channel(false);
            let ended = r.turn("NO REPLAY", Mode::Followup, rx).await.unwrap();
            assert_eq!(ended.status, Status::Ended);
            assert_eq!(r.wire().len(), before);
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn turn_cancel_keeps_child_only_after_confirmed_interrupt_and_tool_completion() {
        for scenario in [
            "continue",
            "continue-adopted-tool-cancel",
            "continue-no-terminal",
            "continue-bad-interrupt",
        ] {
            let resume = scenario.contains("adopted").then_some(Resume {
                id: "parent",
                dynamic_tools: false,
                strict: true,
            });
            let mut r = Rig::open(scenario, resume).await;
            let parent = r.parent().await;
            let (cancel, rx) = watch::channel(false);
            let task = r.turn("HOLD SIDE", Mode::Fresh, rx);
            r.wait(|w| w.iter().any(|v| v["kind"] == "side_active"))
                .await;
            cancel.send(true).unwrap();
            let result = timeout(Duration::from_secs(7), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.status, Status::Interrupted);
            let reusable = !scenario.contains("no-terminal") && !scenario.contains("bad-interrupt");
            assert_eq!(result.retained, reusable, "{scenario}: {}", result.detail);
            if reusable {
                let (_cancel, rx) = watch::channel(false);
                let next = r.turn("FOLLOWUP", Mode::Followup, rx).await.unwrap();
                assert_eq!(next.answer, "side answer 2\n");
                assert!(next.retained);
            } else {
                let (_cancel, rx) = watch::channel(false);
                let next = r.turn("DO NOT FORK", Mode::Followup, rx).await.unwrap();
                assert_eq!(next.status, Status::Ended);
            }
            assert_eq!(
                r.wire()
                    .iter()
                    .filter(|v| v["method"] == "thread/fork")
                    .count(),
                1
            );
            assert!(!r.wire().iter().any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == "parent"));
            assert_eq!(
                r.harness.finish_side().await,
                !scenario.contains("bad-interrupt")
            );
            r.finish(parent).await;
        }
    }
    #[tokio::test]
    async fn incomplete_provider_tools_and_unsolicited_interrupt_never_reuse_child() {
        for scenario in [
            "continue-tool-pending",
            "continue-adopted-tool-pending",
            "continue-unsolicited",
        ] {
            let resume = scenario.contains("adopted").then_some(Resume {
                id: "parent",
                dynamic_tools: false,
                strict: true,
            });
            let mut r = Rig::open(scenario, resume).await;
            let parent = r.parent().await;
            let (_cancel, rx) = watch::channel(false);
            let result = timeout(Duration::from_secs(7), r.turn("SIDE", Mode::Fresh, rx))
                .await
                .unwrap()
                .unwrap();
            assert!(!result.retained);
            let (_cancel, rx) = watch::channel(false);
            assert_eq!(
                r.turn("FOLLOWUP", Mode::Followup, rx).await.unwrap().status,
                Status::Ended
            );
            assert_eq!(
                r.wire()
                    .iter()
                    .filter(|v| v["method"] == "thread/fork")
                    .count(),
                1
            );
            assert!(!parent.is_finished());
            r.finish(parent).await;
        }
    }
}
