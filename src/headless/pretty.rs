//! `bbs headless logs --pretty`: the event log as a readable stream, with
//! markdown and colors when printing to a terminal.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ratatui::backend::IntoCrossterm as _;
use ratatui::crossterm::style::ContentStyle;
use ratatui::style::{Color, Modifier, Style};
use serde_json::{Map, Value};

/// A tool call waiting for its result.
struct Call {
    name: String,
    started: Option<DateTime<Utc>>,
    shown: bool,
}

/// Renders records one at a time, pairing each tool call's records by id.
pub struct Pretty {
    color: bool,
    calls: HashMap<String, Call>,
    /// Ids in start order, so running calls are reported in order.
    order: Vec<String>,
}

impl Pretty {
    pub fn new(color: bool) -> Pretty {
        Pretty {
            color,
            calls: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// One log line as text; empty for hidden records.
    pub fn render(&mut self, line: &str) -> String {
        let Ok(Value::Object(r)) = serde_json::from_str::<Value>(line) else {
            return line.to_string();
        };
        let ts = str_field(&r, "ts");
        let time = ts.get(11..19).unwrap_or(ts);
        let text = str_field(&r, "text");
        let message = str_field(&r, "message");
        let mut out = String::new();
        match str_field(&r, "event") {
            "update" => {}
            "start" => {
                let line = format!(
                    "{time} started {} ({}) in {}",
                    str_field(&r, "agent"),
                    str_field(&r, "harness"),
                    str_field(&r, "cwd")
                );
                self.line(&mut out, &line, dim());
            }
            "session" => {
                let resumed = if r.get("resumed") == Some(&Value::Bool(true)) {
                    " (resumed)"
                } else {
                    ""
                };
                let line = format!("{time} session {}{resumed}", str_field(&r, "id"));
                self.line(&mut out, &line, dim());
            }
            "mail" => {
                let from: Vec<&str> = r
                    .get("from")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let queued = if r.get("queued") == Some(&Value::Bool(true)) {
                    " (queued)"
                } else {
                    ""
                };
                let line = format!("{time} mail from {}{queued}", from.join(", "));
                self.line(&mut out, &line, Style::new().fg(Color::Yellow));
            }
            "prompt" => {
                out.push('\n');
                let header = format!("━━ {time} turn ━━");
                self.line(&mut out, &header, Style::new().fg(Color::Cyan).bold());
                self.block(&mut out, mail_part(text), Style::new());
            }
            "steer" => {
                let line = format!("{time} mail steered into the turn:");
                self.line(&mut out, &line, Style::new().fg(Color::Cyan));
                self.block(&mut out, mail_part(text), Style::new());
            }
            "thought" => {
                for line in text.trim_end().lines() {
                    let line = format!("│ {line}");
                    self.line(
                        &mut out,
                        line.trim_end(),
                        dim().add_modifier(Modifier::ITALIC),
                    );
                }
            }
            "text" => self.markdown(&mut out, text),
            "tool" => self.tool(&mut out, &r),
            "permission" => {
                let line = format!("{time} permission {}", rest(&r));
                self.line(&mut out, &line, Style::new().fg(Color::Yellow).bold());
            }
            "turn_end" => {
                let line = format!("{time} turn end ({})", str_field(&r, "stop_reason"));
                self.line(&mut out, &line, Style::new().fg(Color::Green).bold());
            }
            "turn_error" | "error" => {
                let line = format!("{time} {}: {message}", str_field(&r, "event"));
                self.line(&mut out, &line, Style::new().fg(Color::Red).bold());
            }
            "warning" => {
                let line = format!("{time} warning: {message}");
                self.line(&mut out, &line, Style::new().fg(Color::Yellow).bold());
            }
            "cancel" => self.line(
                &mut out,
                &format!("{time} cancel"),
                Style::new().fg(Color::Yellow),
            ),
            "stop" => match r.get("error").and_then(Value::as_str) {
                Some(err) => {
                    let line = format!("{time} stopped: {err}");
                    self.line(&mut out, &line, Style::new().fg(Color::Red).bold());
                }
                None => self.line(&mut out, &format!("{time} stopped"), Style::new().bold()),
            },
            "unprocessed" => {
                let line = format!("{time} unprocessed mail:");
                self.line(&mut out, &line, Style::new().fg(Color::Yellow).bold());
                self.block(&mut out, text, Style::new());
            }
            event => {
                let line = format!("{time} {event} {}", rest(&r));
                self.line(&mut out, &line, Style::new());
            }
        }
        out
    }

    /// Tool calls still running, each reported once; called while waiting for
    /// more of a followed log, so a long call shows before it finishes.
    pub fn running(&mut self) -> String {
        let mut out = String::new();
        for id in &self.order {
            let call = self.calls.get_mut(id).expect("ordered calls exist");
            if !call.shown {
                call.shown = true;
                let line = format!("  … {} running", call.name);
                out.push_str(&paint(self.color, &line, dim()));
                out.push('\n');
            }
        }
        out
    }

    /// One line per finished call: its name, status, and duration.
    fn tool(&mut self, out: &mut String, r: &Map<String, Value>) {
        let id = str_field(r, "id").to_string();
        let status = str_field(r, "status");
        if status == "pending" || status == "in_progress" {
            if !self.calls.contains_key(&id) {
                self.order.push(id.clone());
                self.calls.insert(
                    id,
                    Call {
                        name: tool_name(str_field(r, "title")),
                        started: str_field(r, "ts").parse().ok(),
                        shown: false,
                    },
                );
            }
            return;
        }
        self.order.retain(|o| *o != id);
        let call = self.calls.remove(&id);
        let name = match &call {
            Some(call) => call.name.clone(),
            None => tool_name(str_field(r, "title")),
        };
        let took = call
            .and_then(|c| c.started)
            .zip(str_field(r, "ts").parse::<DateTime<Utc>>().ok())
            .map(|(start, end)| format!(" ({:.1}s)", (end - start).as_seconds_f64()))
            .unwrap_or_default();
        let (mark, style) = match status {
            "completed" => ("✓", Style::new().fg(Color::Green)),
            "failed" => ("✗", Style::new().fg(Color::Red)),
            _ => ("•", Style::new()),
        };
        let line = format!("  {mark} {name} {status}{took}");
        self.line(out, &line, style);
    }

    fn line(&self, out: &mut String, text: &str, style: Style) {
        out.push_str(&paint(self.color, text, style));
        out.push('\n');
    }

    /// Text in one style, trailing blank lines trimmed.
    fn block(&self, out: &mut String, text: &str, style: Style) {
        for line in text.trim_end().lines() {
            self.line(out, line, style);
        }
    }

    /// Markdown as styled lines; the source text when not coloring.
    fn markdown(&self, out: &mut String, text: &str) {
        if !self.color {
            return self.block(out, text, Style::new());
        }
        for line in tui_markdown::from_str(text.trim_end()).lines {
            for span in &line.spans {
                let style = line.style.patch(span.style);
                out.push_str(&paint(true, &span.content, style));
            }
            out.push('\n');
        }
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn paint(color: bool, text: &str, style: Style) -> String {
    if !color || text.is_empty() || style == Style::new() {
        return text.to_string();
    }
    let style: ContentStyle = style.into_crossterm();
    style.apply(text).to_string()
}

fn str_field<'a>(r: &'a Map<String, Value>, key: &str) -> &'a str {
    r.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The record's fields other than `ts` and `event`, as JSON.
fn rest(r: &Map<String, Value>) -> String {
    let mut r = r.clone();
    r.remove("ts");
    r.remove("event");
    Value::Object(r).to_string()
}

/// A tool's name from its title, without an MCP server prefix
/// (`xd://mcp__bbs_send` → `bbs_send`).
fn tool_name(title: &str) -> String {
    let name = title.rsplit("mcp__").next().unwrap_or(title);
    if name.is_empty() { "tool" } else { name }.to_string()
}

/// The mail in a prompt, without the preamble that precedes it.
fn mail_part(prompt: &str) -> &str {
    let Some(start) = prompt.find("New BBS mail (") else {
        return prompt;
    };
    let mail = &prompt[start..];
    // Skip the "New BBS mail (N messages):" line and the blank line after it.
    mail.split_once("\n\n").map_or(mail, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(lines: &[&str]) -> String {
        let mut pretty = Pretty::new(false);
        lines.iter().map(|l| pretty.render(l)).collect()
    }

    #[test]
    fn turns_show_mail_without_the_preamble() {
        let out = render(&[
            r#"{"ts":"2026-10-16T05:43:05.8Z","event":"mail","count":1,"from":["wbbradley"],"queued":false}"#,
            r#"{"ts":"2026-10-16T05:43:05.9Z","event":"prompt","text":"You are Tim, an agent on BBS.\n\nNew BBS mail (1 message):\n\n[2026-10-16T05:43:05Z] from wbbradley (human) to @g:\nhello\n"}"#,
            r#"{"ts":"2026-10-16T05:43:06Z","event":"update","kind":"usage_update"}"#,
            r#"{"ts":"2026-10-16T05:43:07Z","event":"thought","text":"Let me look.\n"}"#,
            r#"{"ts":"2026-10-16T05:43:10Z","event":"text","text":"**Done.**"}"#,
            r#"{"ts":"2026-10-16T05:43:10Z","event":"turn_end","stop_reason":"end_turn"}"#,
        ]);
        assert_eq!(
            out,
            "05:43:05 mail from wbbradley\n\
             \n━━ 05:43:05 turn ━━\n\
             [2026-10-16T05:43:05Z] from wbbradley (human) to @g:\nhello\n\
             │ Let me look.\n\
             **Done.**\n\
             05:43:10 turn end (end_turn)\n"
        );
    }

    #[test]
    fn tool_calls_are_one_line_when_they_finish() {
        let mut pretty = Pretty::new(false);
        let mut out = String::new();
        for line in [
            r#"{"ts":"2026-10-16T05:43:07.5Z","event":"tool","title":"xd://mcp__bbs_send","kind":"execute","status":"pending","id":"a"}"#,
            r#"{"ts":"2026-10-16T05:43:07.6Z","event":"tool","title":"Read src/main.rs","status":"pending","id":"b"}"#,
            r#"{"ts":"2026-10-16T05:43:07.75Z","event":"tool","status":"failed","id":"a"}"#,
        ] {
            out.push_str(&pretty.render(line));
        }
        assert_eq!(out, "  ✗ bbs_send failed (0.2s)\n");
        // A call still running is reported once while following.
        assert_eq!(pretty.running(), "  … Read src/main.rs running\n");
        assert_eq!(pretty.running(), "");
        let done =
            r#"{"ts":"2026-10-16T05:43:09.6Z","event":"tool","status":"completed","id":"b"}"#;
        assert_eq!(
            pretty.render(done),
            "  ✓ Read src/main.rs completed (2.0s)\n"
        );
    }

    #[test]
    fn problems_and_other_events_stay_visible() {
        let out = render(&[
            r#"{"ts":"2026-10-16T05:00:00Z","event":"warning","message":"slow"}"#,
            r#"{"ts":"2026-10-16T05:00:01Z","event":"turn_error","message":"boom"}"#,
            r#"{"ts":"2026-10-16T05:00:02Z","event":"stop","error":"the harness exited"}"#,
            r#"{"ts":"2026-10-16T05:00:03Z","event":"permission","tool":"bash"}"#,
            "not json\n",
        ]);
        assert_eq!(
            out,
            "05:00:00 warning: slow\n\
             05:00:01 turn_error: boom\n\
             05:00:02 stopped: the harness exited\n\
             05:00:03 permission {\"tool\":\"bash\"}\n\
             not json\n"
        );
    }

    #[test]
    fn colors_and_markdown_only_when_asked() {
        let mut pretty = Pretty::new(true);
        let text =
            pretty.render(r#"{"ts":"2026-10-16T05:00:00Z","event":"text","text":"**bold** text"}"#);
        assert!(text.contains("\x1b["), "{text:?}");
        assert!(!text.contains("**"), "{text:?}");
        let error = pretty.render(r#"{"ts":"2026-10-16T05:00:00Z","event":"error","message":"x"}"#);
        assert!(error.starts_with("\x1b["), "{error:?}");
    }
}
