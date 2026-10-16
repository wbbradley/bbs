//! Sharing activity: with `share_activity = true`, `run` also sends each log
//! record but `update` to the server, where every human can watch it. It is
//! best-effort and at most once: records wait in a small queue, overflow is
//! dropped, and a failure never stops the agent or touches the local log.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::{AGENT_ACTIVITY, Backend};
use crate::config::Config;
use crate::proto;
use crate::service::{Caller, MAX_ACTIVITY_BYTES};

/// Records waiting to be sent; more are dropped.
pub const QUEUE: usize = 256;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A connection that lasted this long resets the backoff.
const STABLE: Duration = Duration::from_secs(60);
/// Marks text cut to fit.
const CUT: &str = "…[cut]";

/// Start sending what arrives on the returned queue, as the agent with
/// `token`. Warnings worth the log (sharing stopped for good) go to `warn`.
pub fn start(
    config: Config,
    dir: PathBuf,
    token: String,
    warn: mpsc::UnboundedSender<String>,
) -> (mpsc::Sender<String>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (tx, tokio::spawn(send(config, dir, token, rx, warn)))
}

/// Connect, send, and reconnect with backoff until the queue closes or the
/// server refuses for good (no `agent-activity.v1`, or a dead token).
async fn send(
    config: Config,
    dir: PathBuf,
    token: String,
    mut rx: mpsc::Receiver<String>,
    warn: mpsc::UnboundedSender<String>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        let err = match connection(&config, &dir, &token, &mut rx).await {
            Ok(()) => return,
            Err(err) => err,
        };
        if permanent(&err) {
            let _ = warn.send(format!("activity is not shared: {err:#}"));
            return;
        }
        tracing::warn!("sharing activity: {err:#}; reconnecting");
        if started.elapsed() >= STABLE {
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One `PostActivity` stream, fed from the queue. `Ok` once the queue closes
/// and what was in flight is sent.
async fn connection(
    config: &Config,
    dir: &std::path::Path,
    token: &str,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let caller = Caller {
        token: Some(token.to_string()),
        ..Default::default()
    };
    let mut backend = Backend::open(config, dir, caller).await?;
    let (tx, stream) = mpsc::channel(QUEUE);
    let events = ReceiverStream::new(stream).map(|record| proto::ActivityEvent { record });
    let post = backend.post_activity(events);
    tokio::pin!(post);
    let mut tx = Some(tx);
    loop {
        tokio::select! {
            posted = &mut post => {
                posted?;
                if tx.is_some() {
                    bail!("the server ended the activity stream");
                }
                return Ok(());
            }
            record = rx.recv(), if tx.is_some() => match record {
                Some(record) => {
                    // A full stream means a stalled connection: drop, as the
                    // queue would.
                    let _ = tx.as_ref().expect("checked").try_send(record);
                }
                None => tx = None,
            },
        }
    }
}

/// Whether retrying cannot help: an older server, or a token that is gone.
fn permanent(err: &anyhow::Error) -> bool {
    if format!("{err:#}").contains(AGENT_ACTIVITY) {
        return true;
    }
    err.chain()
        .filter_map(|e| e.downcast_ref::<tonic::Status>())
        .any(|s| s.code() == tonic::Code::Unauthenticated)
        || format!("{err:#}").contains("unknown or retired token")
}

/// `record` as JSON in at most `MAX_ACTIVITY_BYTES`: its longest strings are
/// cut to fit, and failing that only its time and event are kept.
pub fn capped(record: &Value) -> String {
    let text = record.to_string();
    if text.len() <= MAX_ACTIVITY_BYTES {
        return text;
    }
    let mut record = record.clone();
    // Escaping can make a string longer as JSON, so cut and measure again.
    for _ in 0..8 {
        let text = record.to_string();
        let over = text.len().saturating_sub(MAX_ACTIVITY_BYTES);
        if over == 0 {
            return text;
        }
        let Some(longest) = longest_string(&mut record) else {
            break;
        };
        let mut keep = longest.len().saturating_sub(over + CUT.len());
        while !longest.is_char_boundary(keep) {
            keep -= 1;
        }
        longest.truncate(keep);
        longest.push_str(CUT);
    }
    json!({ "ts": record["ts"], "event": record["event"], "cut": true }).to_string()
}

fn longest_string(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::String(s) => Some(s),
        Value::Array(items) => items
            .iter_mut()
            .filter_map(longest_string)
            .max_by_key(|s| s.len()),
        Value::Object(fields) => fields
            .iter_mut()
            .filter(|(k, _)| !matches!(k.as_str(), "ts" | "event"))
            .filter_map(|(_, v)| longest_string(v))
            .max_by_key(|s| s.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_records_pass_and_long_text_is_cut_to_fit() {
        let small = json!({ "ts": "t", "event": "text", "text": "hi" });
        assert_eq!(capped(&small), small.to_string());

        let long = json!({
            "ts": "t",
            "event": "tool",
            "title": "cat .env",
            "content": [{ "text": "é\"".repeat(MAX_ACTIVITY_BYTES) }],
        });
        let out = capped(&long);
        assert!(out.len() <= MAX_ACTIVITY_BYTES, "{}", out.len());
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["title"], "cat .env");
        assert!(
            parsed["content"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with(CUT)
        );

        // Too many fields to cut one: only the time and event remain.
        let fields: serde_json::Map<String, Value> = (0..2000)
            .map(|i| (format!("k{i}"), json!(i)))
            .chain([("ts".into(), json!("t")), ("event".into(), json!("tool"))])
            .collect();
        let out = capped(&Value::Object(fields));
        assert_eq!(out, r#"{"ts":"t","event":"tool","cut":true}"#);
    }

    #[test]
    fn older_servers_and_dead_tokens_stop_sharing() {
        let missing = anyhow::anyhow!(
            "incompatible BBS server: required capability agent-activity.v1 is unavailable"
        );
        assert!(permanent(&missing));
        let dead = anyhow::Error::new(tonic::Status::unauthenticated("unknown or retired token"));
        assert!(permanent(&dead));
        let down = anyhow::Error::new(tonic::Status::unavailable("server is shutting down"));
        assert!(!permanent(&down));
    }
}
