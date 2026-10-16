//! The local event log (`log.jsonl`) and `bbs headless logs`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::service::fmt_time;
use crate::ui::activity::Activity;
use crate::ui::to_ansi;

/// Appends one JSON object per event. Consecutive text or thought chunks are
/// joined into one record, flushed when another event arrives.
pub struct Log {
    file: File,
    pending: Option<(&'static str, String)>,
    /// With `share_activity`, where records also go, and how many the full
    /// queue dropped.
    share: Option<(tokio::sync::mpsc::Sender<String>, u64)>,
}

impl Log {
    pub fn open(path: &Path) -> Result<Log> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Log {
            file,
            pending: None,
            share: None,
        })
    }

    /// Also send each record but `update` to `share`, cut to fit.
    pub fn share(&mut self, share: tokio::sync::mpsc::Sender<String>) {
        self.share = Some((share, 0));
    }

    /// Stop sharing, so the sender can finish.
    pub fn unshare(&mut self) {
        self.share = None;
    }

    /// Record `event` with `fields` (an object).
    pub fn event(&mut self, event: &str, fields: Value) {
        self.flush();
        self.write(event, fields);
    }

    /// Text the harness streams in chunks (`text` or `thought`).
    pub fn chunk(&mut self, kind: &'static str, text: &str) {
        match &mut self.pending {
            Some((k, buf)) if *k == kind => buf.push_str(text),
            _ => {
                self.flush();
                self.pending = Some((kind, text.to_string()));
            }
        }
    }

    pub fn flush(&mut self) {
        if let Some((kind, text)) = self.pending.take() {
            self.write(kind, json!({ "text": text }));
        }
    }

    fn write(&mut self, event: &str, fields: Value) {
        let mut record = Map::new();
        record.insert("ts".into(), fmt_time(Utc::now()).into());
        record.insert("event".into(), event.into());
        if let Value::Object(fields) = fields {
            record.extend(fields);
        }
        let record = Value::Object(record);
        if let Some((share, dropped)) = &mut self.share
            && event != "update"
            && share
                .try_send(crate::headless::share::capped(&record))
                .is_err()
        {
            *dropped += 1;
            if dropped.is_power_of_two() {
                tracing::warn!("activity sharing is behind; {dropped} records dropped");
            }
        }
        let mut line = record.to_string();
        line.push('\n');
        // Logging is best-effort; a full disk must not stop the agent.
        if let Err(err) = self.file.write_all(line.as_bytes()) {
            tracing::warn!("headless log write failed: {err}");
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Print the log readably; with `follow`, keep printing appended records.
pub async fn print(path: &Path, follow: bool, pretty: bool) -> Result<()> {
    let file = File::open(path).with_context(|| format!("no log at {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let color = std::io::stdout().is_terminal();
    let mut pretty = pretty.then(|| Activity::new(color));
    loop {
        line.clear();
        let pos = reader.stream_position()?;
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            if let Some(pretty) = &mut pretty {
                print!("{}", to_ansi(&pretty.running(), color));
            }
            if !follow {
                return Ok(());
            }
            // A partial line is still being written; read it again later.
            reader.seek(SeekFrom::Start(pos))?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        match &mut pretty {
            Some(pretty) => print!("{}", to_ansi(&pretty.render(&line), color)),
            None => print!("{}", render(&line)),
        }
    }
}

/// One record as text: `HH:MM:SS event: details`.
fn render(line: &str) -> String {
    let Ok(Value::Object(mut r)) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    let ts = r
        .remove("ts")
        .and_then(|v| v.as_str().map(crate::ui::clock))
        .unwrap_or_default();
    let event = r
        .remove("event")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let text = |r: &mut Map<String, Value>, key: &str| {
        r.remove(key)
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    let body = match event.as_str() {
        "text" | "thought" | "prompt" | "unprocessed" => {
            let t = text(&mut r, "text");
            format!("\n{}\n", t.trim_end())
        }
        _ if r.is_empty() => String::new(),
        _ => format!(" {}", Value::Object(r)),
    };
    format!("{ts} {event}:{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_records_but_updates_and_drops_what_the_queue_cannot_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let mut log = Log::open(&path).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        log.share(tx);
        log.event("update", json!({ "kind": "usage" }));
        log.chunk("text", "hel");
        log.chunk("text", "lo");
        log.event("turn_end", json!({ "stop_reason": "end_turn" }));
        // The queue holds two; the third is dropped, but still logged.
        log.event("stop", json!({}));
        assert_eq!(log.share.as_ref().unwrap().1, 1);
        log.unshare();
        let mut shared = Vec::new();
        while let Ok(record) = rx.try_recv() {
            let record: Value = serde_json::from_str(&record).unwrap();
            shared.push(record["event"].as_str().unwrap().to_string());
        }
        assert_eq!(shared, ["text", "turn_end"]);
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(logged.contains("\"event\":\"update\"") && logged.contains("\"event\":\"stop\""));
    }

    #[test]
    fn joins_chunks_and_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        {
            let mut log = Log::open(&path).unwrap();
            log.chunk("text", "Hel");
            log.chunk("text", "lo");
            log.chunk("thought", "hmm");
            log.event("turn_end", json!({ "stop_reason": "end_turn" }));
            log.chunk("text", "tail");
        }
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
        assert_eq!(events, ["text", "thought", "turn_end", "text"]);
        assert_eq!(lines[0]["text"], "Hello");
        assert_eq!(lines[3]["text"], "tail");
        let shown = render(&serde_json::to_string(&lines[2]).unwrap());
        assert!(
            shown.contains("turn_end: {\"stop_reason\":\"end_turn\"}"),
            "{shown}"
        );
    }
}
