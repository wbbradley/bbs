//! A headless agent's event log as styled lines: what `bbs headless logs
//! --pretty` prints.

use crate::ui::theme::{Role, style};
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};

/// A tool call waiting for its result.
struct Call {
    name: String,
    started: Option<DateTime<Utc>>,
    shown: bool,
}

/// One tool call's line, keeping the call's full name so a view can fit it to
/// any width: the mark, then the name, then the status and duration.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolLine {
    mark: Span<'static>,
    name: String,
    status: Span<'static>,
    took: Span<'static>,
}

impl ToolLine {
    pub(crate) fn running(name: &str) -> ToolLine {
        ToolLine {
            mark: Span::styled("  … ", dim()),
            name: name.to_string(),
            status: Span::styled(" running", dim()),
            took: Span::raw(""),
        }
    }

    /// The call as the log prints it, the name's own line breaks kept.
    pub fn full_lines(&self) -> Vec<Line<'static>> {
        styled_parts(vec![
            self.mark.clone(),
            Span::styled(self.name.clone(), style(Role::Tool)),
            self.status.clone(),
            self.took.clone(),
        ])
    }

    /// The call as one line of at most `width` columns: the name's line
    /// breaks become spaces, and a name that does not fit beside the mark,
    /// status and duration ends in an ellipsis.
    pub fn one_line(&self, width: usize) -> Line<'static> {
        let flat: String = self
            .name
            .chars()
            .map(|c| {
                if matches!(c, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        let fixed = self.mark.width() + self.status.width() + self.took.width();
        if fixed < width {
            return Line::from(vec![
                self.mark.clone(),
                Span::styled(fit(&flat, width - fixed), style(Role::Tool)),
                self.status.clone(),
                self.took.clone(),
            ]);
        }
        // Too narrow even for the mark and status: fit the whole call.
        let text = format!(
            "{}{flat}{}{}",
            self.mark.content, self.status.content, self.took.content
        );
        Line::from(Span::styled(fit(&text, width), style(Role::Tool)))
    }
}

/// `text` in at most `width` columns, cut at a grapheme with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for part in text.graphemes(true) {
        if used + part.width() + 1 > width {
            break;
        }
        used += part.width();
        out.push_str(part);
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// Renders records one at a time, pairing each tool call's records by id.
pub struct Activity {
    /// Render `text` records' markdown, rather than showing their source.
    markdown: bool,
    /// Show times in local time rather than the log's UTC.
    local: bool,
    /// Mark turns: a separator before each prompt and a `turn end` line.
    turns: bool,
    /// Render reasoning Markdown without blank display lines in connect.
    compact_thoughts: bool,
    calls: HashMap<String, Call>,
    /// Ids in start order, so running calls are reported in order.
    order: Vec<String>,
    /// The call the last record finished, for a view that fits it itself.
    finished: Option<ToolLine>,
}

impl Activity {
    pub fn new(markdown: bool) -> Activity {
        Activity {
            markdown,
            local: false,
            turns: true,
            compact_thoughts: false,
            calls: HashMap::new(),
            order: Vec::new(),
            finished: None,
        }
    }

    /// Show times in local time, as `bbs connect` does elsewhere.
    pub fn local_time(mut self) -> Activity {
        self.local = true;
        self
    }

    /// Leave out turn markers, the prompt separator and `turn end`, where
    /// something else shows turn state, as `bbs connect`'s progress mark does.
    pub fn without_turns(mut self) -> Activity {
        self.turns = false;
        self
    }

    /// Use compact Markdown reasoning in the conversation, leaving logs unchanged.
    pub fn compact_thoughts(mut self) -> Activity {
        self.compact_thoughts = true;
        self
    }

    /// One log line as styled lines; none for hidden records.
    pub fn render(&mut self, line: &str) -> Vec<Line<'static>> {
        self.finished = None;
        let Ok(Value::Object(r)) = serde_json::from_str::<Value>(line) else {
            return styled(line.trim_end_matches('\n'), style(Role::Text));
        };
        let time = super::clock(str_field(&r, "ts"), self.local);
        let text = str_field(&r, "text");
        let message = str_field(&r, "message");
        // A missing tool result cannot keep a call running into another turn
        // or runner session. Forget its live state without inventing a result.
        if matches!(
            str_field(&r, "event"),
            "start" | "session" | "prompt" | "turn_end" | "turn_error" | "cancel" | "stop"
        ) {
            self.calls.clear();
            self.order.clear();
        }
        let mut out = Vec::new();
        match str_field(&r, "event") {
            "update" => {}
            "start" => {
                out.extend(styled_parts(vec![
                    Span::styled(time.clone(), style(Role::Timestamp)),
                    Span::styled(" started ", dim()),
                    Span::styled(str_field(&r, "agent").to_string(), style(Role::Agent)),
                    Span::styled(" (", dim()),
                    Span::styled(str_field(&r, "harness").to_string(), style(Role::Technical)),
                    Span::styled(") in ", dim()),
                    Span::styled(str_field(&r, "cwd").to_string(), style(Role::Technical)),
                ]));
            }
            "session" => {
                let resumed = if r.get("resumed") == Some(&Value::Bool(true)) {
                    " (resumed)"
                } else {
                    ""
                };
                self.timed(
                    &mut out,
                    &time,
                    &format!("session {}{resumed}", str_field(&r, "id")),
                    dim(),
                );
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
                out.extend(styled_parts(vec![
                    Span::styled(time.clone(), style(Role::Timestamp)),
                    Span::styled(" mail from ", style(Role::Warning)),
                    Span::styled(from.join(", "), style(Role::Text)),
                    Span::styled(queued, style(Role::Warning)),
                ]));
            }
            "prompt" => {
                if self.turns {
                    out.push(Line::default());
                    out.extend(styled_parts(vec![
                        Span::styled("━━ ", style(Role::Accent).bold()),
                        Span::styled(time.clone(), style(Role::Timestamp)),
                        Span::styled(" turn ━━", style(Role::Accent).bold()),
                    ]));
                }
                self.block(&mut out, mail_part(text), style(Role::Text));
            }
            "steer" => {
                self.timed(
                    &mut out,
                    &time,
                    "mail steered into the turn:",
                    style(Role::Accent),
                );
                self.block(&mut out, mail_part(text), style(Role::Text));
            }
            "thought" if self.compact_thoughts => self.thought(&mut out, text),
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
                let line = format!("permission {}", rest(&r));
                self.timed(&mut out, &time, &line, style(Role::Warning).bold());
            }
            "turn_end" if !self.turns => {}
            "turn_end" => {
                let line = format!("turn end ({})", str_field(&r, "stop_reason"));
                self.timed(&mut out, &time, &line, style(Role::Success).bold());
            }
            "turn_error" | "error" => {
                let line = format!("{}: {message}", str_field(&r, "event"));
                self.timed(&mut out, &time, &line, style(Role::Error).bold());
            }
            "warning" => {
                let line = format!("warning: {message}");
                self.timed(&mut out, &time, &line, style(Role::Warning).bold());
            }
            "cancel" => self.timed(&mut out, &time, "cancel", style(Role::Warning)),
            "stop" => match r.get("error").and_then(Value::as_str) {
                Some(err) => {
                    let line = format!("stopped: {err}");
                    self.timed(&mut out, &time, &line, style(Role::Error).bold());
                }
                None => self.timed(&mut out, &time, "stopped", style(Role::Text).bold()),
            },
            "unprocessed" => {
                let line = format!("unprocessed mail:");
                self.timed(&mut out, &time, &line, style(Role::Warning).bold());
                self.block(&mut out, text, style(Role::Text));
            }
            event => {
                let line = format!("{event} {}", rest(&r));
                self.timed(&mut out, &time, &line, style(Role::Text));
            }
        }
        out
    }

    /// Tool calls still running, each reported once; called while waiting for
    /// more of a followed log, so a long call shows before it finishes.
    /// Every tool call still running, one line each, in start order, without
    /// marking them shown: for a view redrawn as calls finish.
    #[cfg(test)]
    pub fn in_progress(&self) -> Vec<Line<'static>> {
        self.running_calls()
            .iter()
            .flat_map(|call| call.full_lines())
            .collect()
    }

    /// Every tool call still running, in start order, for a view to fit.
    pub fn running_calls(&self) -> Vec<ToolLine> {
        self.order
            .iter()
            .filter_map(|id| self.calls.get(id))
            .map(|call| ToolLine::running(&call.name))
            .collect()
    }

    /// The call the last rendered record finished, if it finished one.
    pub fn take_tool(&mut self) -> Option<ToolLine> {
        self.finished.take()
    }

    pub fn running(&mut self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for id in &self.order {
            let call = self.calls.get_mut(id).expect("ordered calls exist");
            if !call.shown {
                call.shown = true;
                out.extend(running_line(&call.name));
            }
        }
        out
    }

    /// One line per finished call: its name, status, and duration.
    fn tool(&mut self, out: &mut Vec<Line<'static>>, r: &Map<String, Value>) {
        let id = str_field(r, "id").to_string();
        let status = str_field(r, "status");
        let title = str_field(r, "title");
        if !title.is_empty()
            && let Some(call) = self.calls.get_mut(&id)
        {
            call.name = tool_name(title);
        }
        // ACP tool updates are partial: output or title updates need not carry
        // a status. They do not finish a call or establish a new running call.
        if status.is_empty() {
            return;
        }
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
        let (mark, status_style) = match status {
            "completed" => ("✓", style(Role::Success)),
            "failed" => ("✗", style(Role::Error)),
            _ => ("•", style(Role::Text)),
        };
        let call = ToolLine {
            mark: Span::styled(format!("  {mark} "), status_style),
            name,
            status: Span::styled(format!(" {status}"), status_style),
            took: Span::styled(took, style(Role::Timestamp)),
        };
        out.extend(call.full_lines());
        self.finished = Some(call);
    }

    fn timed(&self, out: &mut Vec<Line<'static>>, time: &str, text: &str, status: Style) {
        out.extend(styled_parts(vec![
            Span::styled(time.to_string(), style(Role::Timestamp)),
            Span::styled(format!(" {text}"), status),
        ]));
    }

    fn line(&self, out: &mut Vec<Line<'static>>, text: &str, style: Style) {
        out.extend(styled(text, style));
    }

    /// Text in one style, trailing blank lines trimmed.
    fn block(&self, out: &mut Vec<Line<'static>>, text: &str, style: Style) {
        for line in text.trim_end().lines() {
            self.line(out, line, style);
        }
    }

    fn thought(&self, out: &mut Vec<Line<'static>>, text: &str) {
        // Parse before removing blank display lines so paragraph and code syntax
        // retain their meaning. Only the rendered conversation is compacted.
        for line in crate::ui::markdown::render(text.trim()).lines {
            if line.spans.iter().all(|span| span.content.trim().is_empty()) {
                continue;
            }
            let mut spans = vec![Span::styled("│ ", dim().italic())];
            spans.extend(line.spans.iter().map(|span| {
                let mut rendered = line.style.patch(span.style);
                if rendered.fg == style(Role::Text).fg {
                    rendered.fg = dim().fg;
                }
                Span::styled(span.content.to_string(), dim().italic().patch(rendered))
            }));
            out.push(Line::from(spans));
        }
    }

    /// Markdown as styled lines, or the source text.
    fn markdown(&self, out: &mut Vec<Line<'static>>, text: &str) {
        if !self.markdown {
            return self.block(out, text, style(Role::Text));
        }
        for line in crate::ui::markdown::render(text.trim_end()).lines {
            let spans = line
                .spans
                .iter()
                .map(|span| Span::styled(span.content.to_string(), line.style.patch(span.style)));
            out.push(Line::from(spans.collect::<Vec<_>>()));
        }
    }
}

fn running_line(name: &str) -> Vec<Line<'static>> {
    ToolLine::running(name).full_lines()
}

/// Structured values keep their categories, even when a field contains newlines.
fn styled_parts(spans: Vec<Span<'static>>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for span in spans {
        for (i, part) in span.content.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            lines
                .last_mut()
                .expect("one line")
                .spans
                .push(Span::styled(part.to_string(), span.style));
        }
    }
    lines
}

/// `text` in one style, one `Line` per line of it: a `Line` drops newlines.
fn styled(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|line| Line::styled(line.to_string(), style))
        .collect()
}

fn dim() -> Style {
    style(Role::Muted)
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
    use crate::ui::to_ansi;

    fn render(lines: &[&str]) -> String {
        let mut activity = Activity::new(false);
        lines
            .iter()
            .map(|l| to_ansi(&activity.render(l), false))
            .collect()
    }

    #[test]
    fn structured_activity_times_agents_and_tools_are_independently_styled() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut renderer = Activity::new(false);
                let start = renderer.render(r#"{"event":"start","agent":"Mateo","harness":"codex","cwd":"/src","ts":"2026-10-18T12:00:00Z"}"#);
                assert_eq!(
                    start[0].to_string(),
                    "12:00:00 started Mateo (codex) in /src"
                );
                assert_eq!(start[0].spans[0].style, style(Role::Timestamp));
                assert_eq!(
                    start[0]
                        .spans
                        .iter()
                        .find(|s| s.content == "Mateo")
                        .unwrap()
                        .style,
                    style(Role::Agent)
                );
                let warning = renderer.render(
                    r#"{"event":"warning","message":"Mateo said ada","ts":"2026-10-18T12:00:00Z"}"#,
                );
                assert_eq!(warning[0].spans[0].style, style(Role::Timestamp));
                assert_eq!(warning[0].spans[1].style, style(Role::Warning).bold());
                renderer.render(r#"{"event":"tool","id":"a","title":"shell","status":"pending","ts":"2026-10-18T12:00:00Z"}"#);
                let running = renderer.in_progress();
                assert_eq!(
                    running[0]
                        .spans
                        .iter()
                        .find(|s| s.content == "shell")
                        .unwrap()
                        .style,
                    style(Role::Tool)
                );
                let done = renderer.render(
                    r#"{"event":"tool","id":"a","status":"completed","ts":"2026-10-18T12:00:02Z"}"#,
                );
                assert_eq!(done[0].to_string(), "  ✓ shell completed (2.0s)");
                assert_eq!(done[0].spans[1].style, style(Role::Tool));
                assert_eq!(done[0].spans[3].style, style(Role::Timestamp));
                assert_eq!(
                    super::super::to_ansi(&done, false),
                    "  ✓ shell completed (2.0s)\n"
                );
            });
        }
    }

    #[test]
    fn compact_reasoning_keeps_markdown_styles_and_code_indentation() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        let record = serde_json::json!({"event":"thought", "text":
            "\n**Planning** and *checking* `config`\n\n[docs](https://example.com)\n\n- first\n- second\n\n```python\nif ready:\n    run()\n\n    finish()\n```\n"}).to_string();
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut renderer = Activity::new(true).compact_thoughts();
                let lines = renderer.render(&record);
                let text = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
                assert!(
                    text.iter()
                        .all(|line| line.starts_with("│ ") && !line[4..].trim().is_empty()),
                    "{text:?}"
                );
                assert!(
                    text.contains(&"│ Planning and checking config".into()),
                    "{text:?}"
                );
                assert!(text.contains(&"│     run()".into()), "{text:?}");
                assert!(text.contains(&"│     finish()".into()), "{text:?}");
                let span = |name: &str| {
                    lines
                        .iter()
                        .flat_map(|l| &l.spans)
                        .find(|s| s.content == name)
                        .unwrap()
                };
                assert!(span("Planning").style.add_modifier.contains(Modifier::BOLD));
                assert!(
                    span("checking")
                        .style
                        .add_modifier
                        .contains(Modifier::ITALIC)
                );
                assert_eq!(span("config").style.fg, style(Role::Technical).fg);
                assert!(
                    span("docs")
                        .style
                        .add_modifier
                        .contains(Modifier::UNDERLINED)
                );
                assert!(
                    renderer
                        .render(r#"{"event":"thought","text":" \n\t"}"#)
                        .is_empty()
                );
                for markdown in [false, true] {
                    let pretty = Activity::new(markdown).render(&record);
                    assert!(
                        pretty
                            .iter()
                            .any(|line| line.to_string().contains("**Planning**"))
                    );
                    assert!(pretty.iter().any(|line| line.to_string() == "│"));
                }
            });
        }
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
    fn without_turns_keeps_the_mail_and_drops_only_the_markers() {
        let prompt = r#"{"ts":"2026-10-16T05:43:05.9Z","event":"prompt","text":"You are Tim.\n\nNew BBS mail (1 message):\n\n[2026-10-16T05:43:05Z] from wbbradley (human) to @g:\nhello\n"}"#;
        let end = r#"{"ts":"2026-10-16T05:43:10Z","event":"turn_end","stop_reason":"end_turn"}"#;
        let mut quiet = Activity::new(false).without_turns();
        assert_eq!(
            to_ansi(&quiet.render(prompt), false),
            "[2026-10-16T05:43:05Z] from wbbradley (human) to @g:\nhello\n"
        );
        assert!(quiet.render(end).is_empty());
        // Errors and stops are not turn markers.
        let error = r#"{"ts":"2026-10-16T05:43:11Z","event":"turn_error","message":"boom"}"#;
        assert!(to_ansi(&quiet.render(error), false).contains("turn_error: boom"));
    }

    #[test]
    fn a_fitted_tool_call_is_one_line_within_the_width() {
        use unicode_width::UnicodeWidthStr;
        let finish = |title: &str| {
            let mut pretty = Activity::new(false);
            let start = serde_json::json!({"ts":"2026-10-16T05:00:00Z","event":"tool","id":"a","title":title,"status":"in_progress"});
            pretty.render(&start.to_string());
            let end =
                r#"{"ts":"2026-10-16T05:00:01Z","event":"tool","id":"a","status":"completed"}"#;
            let full = to_ansi(&pretty.render(end), false);
            (full, pretty.take_tool().expect("a finished call"))
        };
        let text = |line: Line<'static>| line.to_string();
        // A multi-line Python invocation: the log keeps it whole; the fitted
        // line flattens it, ends the name with an ellipsis and keeps the rest.
        let script = "python3 - <<'EOF'\nimport json\nfor i in range(100):\n    print(json.dumps({'i': i}))\nEOF";
        let (full, call) = finish(script);
        assert_eq!(
            full,
            format!("  ✓ {script} completed (1.0s)\n"),
            "pretty logs keep the call's own lines"
        );
        for width in [80, 40, 24] {
            let line = text(call.one_line(width));
            assert!(line.width() <= width, "{width}: {line}");
            assert!(line.ends_with("… completed (1.0s)"), "{width}: {line}");
            assert!(!line.contains('\n'));
        }
        let wide = text(call.one_line(200));
        assert!(wide.contains("import json for i in range(100):"), "{wide}");
        assert!(!wide.contains('…'), "fits whole: {wide}");
        // Wide graphemes and combining marks are never split or overrun.
        let (_, call) = finish(&"漢字👩‍💻é\u{301}".repeat(20));
        for width in [80, 33, 25] {
            let line = text(call.one_line(width));
            assert!(line.width() <= width, "{width}: {line}");
            assert!(line.ends_with("… completed (1.0s)"), "{width}: {line}");
        }
        // Too narrow even for the mark and status: still within the width.
        for width in [0, 5, 12] {
            assert!(text(call.one_line(width)).width() <= width);
        }
        // A running call fits the same way.
        let running = ToolLine::running(script);
        let line = text(running.one_line(30));
        assert!(line.width() <= 30 && line.ends_with("… running"), "{line}");
    }

    #[test]
    fn only_the_record_that_finishes_a_call_carries_it() {
        let mut pretty = Activity::new(false);
        pretty.render(r#"{"ts":"2026-10-16T05:00:00Z","event":"tool","id":"a","title":"ls","status":"in_progress"}"#);
        assert!(pretty.take_tool().is_none(), "a start finishes nothing");
        pretty.render(
            r#"{"ts":"2026-10-16T05:00:01Z","event":"tool","id":"a","status":"completed"}"#,
        );
        assert!(pretty.take_tool().is_some());
        pretty.render(
            r#"{"ts":"2026-10-16T05:00:01Z","event":"tool","id":"a","status":"completed"}"#,
        );
        // Records after it never inherit the call, taken or not.
        for later in [
            r#"{"ts":"2026-10-16T05:00:02Z","event":"text","text":"after"}"#,
            r#"{"ts":"2026-10-16T05:00:03Z","event":"tool","id":"b","title":"partial"}"#,
            "not json",
        ] {
            pretty.render(later);
            assert!(pretty.take_tool().is_none(), "{later}");
        }
    }

    #[test]
    fn times_are_the_logs_utc_unless_local() {
        let line = r#"{"ts":"2026-10-16T05:43:10Z","event":"turn_end","stop_reason":"end_turn"}"#;
        let utc = to_ansi(&Activity::new(false).render(line), false);
        assert_eq!(utc, "05:43:10 turn end (end_turn)\n");
        let local = to_ansi(&Activity::new(false).local_time().render(line), false);
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-16T05:43:10Z")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S");
        assert_eq!(local, format!("{expected} turn end (end_turn)\n"));
    }

    #[test]
    fn tool_calls_are_one_line_when_they_finish() {
        let mut pretty = Activity::new(false);
        let mut out = String::new();
        for line in [
            r#"{"ts":"2026-10-16T05:43:07.5Z","event":"tool","title":"xd://mcp__bbs_send","kind":"execute","status":"pending","id":"a"}"#,
            r#"{"ts":"2026-10-16T05:43:07.6Z","event":"tool","title":"Read src/main.rs","status":"pending","id":"b"}"#,
            r#"{"ts":"2026-10-16T05:43:07.75Z","event":"tool","status":"failed","id":"a"}"#,
        ] {
            out.push_str(&to_ansi(&pretty.render(line), false));
        }
        assert_eq!(out, "  ✗ bbs_send failed (0.2s)\n");
        // A call still running is reported once while following.
        assert_eq!(
            to_ansi(&pretty.running(), false),
            "  … Read src/main.rs running\n"
        );
        assert!(pretty.running().is_empty());
        let done =
            r#"{"ts":"2026-10-16T05:43:09.6Z","event":"tool","status":"completed","id":"b"}"#;
        assert_eq!(
            to_ansi(&pretty.render(done), false),
            "  ✓ Read src/main.rs completed (2.0s)\n"
        );
    }

    #[test]
    fn lifecycle_boundaries_clear_tools_without_inventing_results() {
        for turns in [true, false] {
            for event in [
                "start",
                "session",
                "prompt",
                "turn_end",
                "turn_error",
                "cancel",
                "stop",
            ] {
                let mut pretty = Activity::new(false);
                pretty.turns = turns;
                pretty.render(r#"{"event":"tool","status":"pending","id":"a","title":"old call"}"#);
                pretty.render(
                    r#"{"event":"tool","status":"in_progress","id":"b","title":"another call"}"#,
                );
                assert_eq!(pretty.running_calls().len(), 2);
                let out = pretty.render(&serde_json::json!({"event": event}).to_string());
                assert!(pretty.running_calls().is_empty(), "{event}, turns={turns}");
                assert!(pretty.running().is_empty());
                assert!(pretty.take_tool().is_none());
                let text = to_ansi(&out, false);
                assert!(!text.contains("completed") && !text.contains("old call"));
                // A later call can reuse a provider id without retaining the
                // previous runner's title or suppressing its running notice.
                pretty.render(r#"{"event":"tool","status":"pending","id":"a","title":"new call"}"#);
                assert_eq!(to_ansi(&pretty.running(), false), "  … new call running\n");
            }
        }
    }

    #[test]
    fn ordinary_activity_and_steering_keep_live_tools() {
        let mut pretty = Activity::new(false);
        pretty.render(r#"{"event":"tool","status":"pending","id":"a","title":"live call"}"#);
        for event in [
            "mail",
            "steer",
            "thought",
            "text",
            "warning",
            "permission",
            "update",
            "error",
        ] {
            pretty.render(&serde_json::json!({"event":event}).to_string());
            assert_eq!(pretty.running_calls().len(), 1, "{event}");
        }
    }

    #[test]
    fn partial_tool_updates_keep_the_title_and_original_start_time() {
        let mut pretty = Activity::new(false);
        for line in [
            r#"{"ts":"2026-10-16T05:00:00Z","event":"tool","title":"cargo test","status":"in_progress","id":"a"}"#,
            r#"{"ts":"2026-10-16T05:00:01Z","event":"tool","id":"a"}"#,
            r#"{"ts":"2026-10-16T05:00:02Z","event":"tool","id":"a"}"#,
        ] {
            assert!(pretty.render(line).is_empty());
        }
        assert_eq!(
            to_ansi(&pretty.in_progress(), false),
            "  … cargo test running\n"
        );
        assert_eq!(
            to_ansi(&pretty.running(), false),
            "  … cargo test running\n"
        );
        assert!(pretty.running().is_empty());
        let done = r#"{"ts":"2026-10-16T05:00:03Z","event":"tool","status":"completed","id":"a"}"#;
        assert_eq!(
            to_ansi(&pretty.render(done), false),
            "  ✓ cargo test completed (3.0s)\n"
        );
        assert!(pretty.in_progress().is_empty());
        // An output update after completion must not resurrect the call.
        assert!(pretty.render(r#"{"event":"tool","id":"a"}"#).is_empty());
        assert!(pretty.in_progress().is_empty());
    }

    #[test]
    fn tool_titles_can_arrive_in_later_updates() {
        let mut pretty = Activity::new(false);
        assert!(
            pretty
                .render(r#"{"event":"tool","status":"pending","id":"a"}"#)
                .is_empty()
        );
        for update in [
            r#"{"event":"tool","title":"Read first.rs","status":"in_progress","id":"a"}"#,
            r#"{"event":"tool","title":"Read second.rs","id":"a"}"#,
        ] {
            assert!(pretty.render(update).is_empty());
        }
        assert_eq!(
            to_ansi(&pretty.in_progress(), false),
            "  … Read second.rs running\n"
        );
        let done = r#"{"event":"tool","title":"Read final.rs","status":"failed","id":"a"}"#;
        assert_eq!(
            to_ansi(&pretty.render(done), false),
            "  ✗ Read final.rs failed\n"
        );
        assert!(pretty.in_progress().is_empty());
        // A history window may begin with a result whose start was dropped.
        let orphan = r#"{"event":"tool","title":"Read orphan.rs","status":"completed","id":"b"}"#;
        assert_eq!(
            to_ansi(&pretty.render(orphan), false),
            "  ✓ Read orphan.rs completed\n"
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
    fn multi_line_titles_keep_their_lines() {
        let out = render(&[
            r#"{"ts":"2026-10-16T05:00:00Z","event":"tool","title":"$ cat <<EOF\nhi\nEOF","status":"pending","id":"a"}"#,
            r#"{"ts":"2026-10-16T05:00:01Z","event":"tool","status":"completed","id":"a"}"#,
        ]);
        assert_eq!(out, "  ✓ $ cat <<EOF\nhi\nEOF completed (1.0s)\n");
    }

    #[test]
    fn colors_and_markdown_only_when_asked() {
        let mut pretty = Activity::new(true);
        let text = to_ansi(
            &pretty
                .render(r#"{"ts":"2026-10-16T05:00:00Z","event":"text","text":"**bold** text"}"#),
            true,
        );
        assert!(text.contains("\x1b["), "{text:?}");
        assert!(!text.contains("**"), "{text:?}");
        let error = to_ansi(
            &pretty.render(r#"{"ts":"2026-10-16T05:00:00Z","event":"error","message":"x"}"#),
            true,
        );
        assert!(error.starts_with("\x1b["), "{error:?}");
    }
}
