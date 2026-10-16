//! The local event log (`log.jsonl`) and `bbs headless logs`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{Map, Value, json};

use super::pretty::Pretty;
use crate::service::fmt_time;

/// Appends one JSON object per event. Consecutive text or thought chunks are
/// joined into one record, flushed when another event arrives.
pub struct Log {
    file: File,
    pending: Option<(&'static str, String)>,
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
        })
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
        let mut line = Value::Object(record).to_string();
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
    let mut pretty = pretty.then(|| Pretty::new(std::io::stdout().is_terminal()));
    loop {
        line.clear();
        let pos = reader.stream_position()?;
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            if let Some(pretty) = &mut pretty {
                print!("{}", pretty.running());
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
            Some(pretty) => print!("{}", pretty.render(&line)),
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
        .and_then(|v| v.as_str().map(|s| s.get(11..19).unwrap_or(s).to_string()))
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
