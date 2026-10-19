//! JSON-RPC over a harness subprocess's stdio, shared by the ACP and Codex
//! clients. Responses resolve their calls; everything else the harness sends
//! goes, in order, to the client's handler.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::time::Duration;

/// How long the harness's process group gets to exit after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The harness process and its process group, which also holds every command
/// the agent started. Dropping it kills the group.
pub struct Process {
    child: Child,
    pgid: libc::pid_t,
    reader: tokio::task::JoinHandle<()>,
}

impl Process {
    /// Wait for the harness itself to exit.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// SIGTERM the group, give it `STOP_GRACE` to exit, then SIGKILL whatever
    /// remains.
    pub async fn stop(&mut self) -> bool {
        signal_group(self.pgid, libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while tokio::time::Instant::now() < deadline {
            // Reap the harness, so it does not keep the group alive as a zombie.
            let _ = self.child.try_wait();
            if !group_alive(self.pgid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        signal_group(self.pgid, libc::SIGKILL);
        let _ = self.child.wait().await;
        // EOF follows the final protocol bytes. Join their reader before the
        // caller seals/drains owner output, including a cancellation tail.
        let drained = matches!(
            tokio::time::timeout(Duration::from_secs(1), &mut self.reader).await,
            Ok(Ok(()))
        );
        if !drained {
            self.reader.abort();
        }
        drained
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        signal_group(self.pgid, libc::SIGKILL);
        self.reader.abort();
    }
}

fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill(2) with a negative pid signals that process group; it has no
    // memory-safety preconditions.
    unsafe {
        libc::kill(-pgid, signal);
    }
}

/// Whether any process remains in the group.
fn group_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: as above; signal 0 only checks that the group exists.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// A message from the harness other than a response.
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// The harness's stdout closed; pending calls have failed.
    Exited,
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;

struct Inner {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
}

/// The client end of the connection.
#[derive(Clone)]
pub struct Rpc {
    inner: Arc<Inner>,
}

impl Rpc {
    /// Start `command` in `cwd` in its own process group, with stderr appended
    /// to `stderr`. `handler` sees every request and notification in order; it
    /// must not block, so it answers requests from spawned tasks.
    pub fn spawn(
        command: &[String],
        env: &std::collections::BTreeMap<String, String>,
        cwd: &Path,
        stderr: &Path,
        handler: impl FnMut(&Rpc, Incoming) + Send + 'static,
    ) -> Result<(Rpc, Process)> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(stderr)
            .with_context(|| format!("opening {}", stderr.display()))?;
        stderr.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        // Its own process group: a terminal's Ctrl-C reaches bbs only, and stop
        // can signal the agent's commands along with the harness.
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting harness {command:?}"))?;
        let pgid = child.id().context("the harness exited at once")? as libc::pid_t;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let rpc = Rpc {
            inner: Arc::new(Inner {
                stdin: Mutex::new(stdin),
                pending: Default::default(),
                next_id: AtomicU64::new(1),
            }),
        };
        let reader = tokio::spawn(read_loop(rpc.clone(), BufReader::new(stdout), handler));
        Ok((
            rpc,
            Process {
                child,
                pgid,
                reader,
            },
        ))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.call(method, params)
            .await
            .map_err(|e| anyhow!("{method}: {} (code {})", e.message, e.code))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = self.send(&msg).await {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(RpcError {
                code: 0,
                message: format!("{err:#}"),
            });
        }
        rx.await.unwrap_or_else(|_| {
            Err(RpcError {
                code: 0,
                message: "the harness exited".into(),
            })
        })
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Answer the harness's request `id`; an `Err` is a method-not-found error.
    pub async fn respond(&self, id: Value, reply: Result<Value, String>) -> Result<()> {
        let response = match reply {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": message },
            }),
        };
        self.send(&response).await
    }

    /// Answer from a spawned task, so the read loop never waits on stdin.
    pub fn respond_later(&self, id: Value, reply: Result<Value, String>) {
        let rpc = self.clone();
        tokio::spawn(async move {
            let _ = rpc.respond(id, reply).await;
        });
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let mut line = msg.to_string();
        line.push('\n');
        let mut stdin = self.inner.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

async fn read_loop(
    rpc: Rpc,
    mut stdout: BufReader<ChildStdout>,
    mut handler: impl FnMut(&Rpc, Incoming),
) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(mut msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("harness wrote a non-JSON line");
            continue;
        };
        let method = msg["method"].as_str().map(str::to_string);
        let params = msg["params"].take();
        match (method, msg.get("id")) {
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let Some(tx) = rpc.inner.pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let result = match msg.get("error") {
                    Some(err) => Err(RpcError {
                        code: err["code"].as_i64().unwrap_or(0),
                        message: err["message"].as_str().unwrap_or("error").to_string(),
                    }),
                    None => Ok(msg["result"].take()),
                };
                let _ = tx.send(result);
            }
            (Some(method), Some(id)) => handler(
                &rpc,
                Incoming::Request {
                    id: id.clone(),
                    method,
                    params,
                },
            ),
            (Some(method), None) => handler(&rpc, Incoming::Notification { method, params }),
            (None, None) => {}
        }
    }
    for (_, tx) in rpc.inner.pending.lock().unwrap().drain() {
        let _ = tx.send(Err(RpcError {
            code: 0,
            message: "the harness exited".into(),
        }));
    }
    handler(&rpc, Incoming::Exited);
}
