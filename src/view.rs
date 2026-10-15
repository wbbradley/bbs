//! `bbs view`: a read-only feed of messages on the board, as an auto-following
//! TUI, plain text, or JSONL. It never archives anything, so watching does not
//! consume anyone's mail.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde::Serialize;

use crate::app::{display_time, parse_time};
use crate::backend::{Backend, FeedWatch};
use crate::cli::ViewArgs;
use crate::proto::{FeedRequest, Kind, Message};

const BACKLOG: u32 = 200;
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn run(backend: &mut Backend, args: ViewArgs) -> Result<()> {
    // Nothing sent from now on is before an --until bound, so following one
    // would wait forever: --until prints the history and exits.
    let follow = !args.no_follow && args.until.is_none();
    let tui = !args.json && follow;
    if tui && !(std::io::stdout().is_terminal() && std::io::stdin().is_terminal()) {
        bail!("bbs view needs a terminal; use --json or --no-follow for plain output");
    }
    let req = request(&args, Utc::now())?;
    if args.json {
        return stream_json(backend, req, follow).await;
    }
    let mut feed = backend.watch_feed(req, false).await?;
    // Fetch before taking over the terminal, so a refusal prints normally.
    let rows = feed.next().await?;
    if !follow {
        write_out(&format_feed(&rows))?;
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = feed_loop(&mut terminal, &mut feed, rows).await;
    ratatui::restore();
    result
}

/// Translate flags into the server-side request. `--from @human` selects the
/// human and every agent they own; a bare name selects that one sender.
fn request(args: &ViewArgs, now: DateTime<Utc>) -> Result<FeedRequest> {
    let (from, from_human) = match args.from.as_deref() {
        Some(from) => match from.strip_prefix('@') {
            Some(human) => (None, Some(human.to_string())),
            None => (Some(from.to_string()), None),
        },
        None => (None, None),
    };
    let time = |flag, value: &Option<String>| {
        value
            .as_deref()
            .map(|v| parse_time(flag, v, now))
            .transpose()
    };
    Ok(FeedRequest {
        backlog: args.limit.unwrap_or(BACKLOG),
        group: args.group.clone(),
        from,
        from_human,
        since: time("--since", &args.since)?,
        until: time("--until", &args.until)?,
        mine: args.mine,
        after_id: None,
    })
}

/// Write to stdout; `false` once the reader has gone away (e.g. `| head`).
fn write_out(text: &str) -> Result<bool> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e).context("writing to stdout"),
    }
}

/// Emit the matching history as JSONL, then, when following, every newer
/// matching message exactly once. Following reads by cursor (`after_id`), not
/// by diffing snapshots, so bursts larger than the window are never skipped.
/// The history request already requires `feed-filter.v1` when following, so an
/// older server refuses before anything is printed.
async fn stream_json(backend: &mut Backend, req: FeedRequest, follow: bool) -> Result<()> {
    let history = backend
        .watch_feed(req.clone(), follow)
        .await?
        .next()
        .await?;
    if !write_out(&json_lines(&history)?)? || !follow {
        return Ok(());
    }
    // An empty window means nothing matched yet, so everything after id 0 is new.
    let mut last = history.last().map_or(0, |m| m.id);
    let tail = FeedRequest {
        after_id: Some(last),
        ..req
    };
    let mut feed = backend.watch_feed(tail, true).await?;
    loop {
        let batch = feed.next().await?;
        if let Some(m) = batch.iter().find(|m| m.id <= last) {
            bail!(
                "feed cursor went backwards: got message {} after {last}",
                m.id
            );
        }
        if !write_out(&json_lines(&batch)?)? {
            return Ok(());
        }
        last = batch.last().map_or(last, |m| m.id);
    }
}

fn json_lines(rows: &[Message]) -> Result<String> {
    let mut out = String::new();
    for m in rows {
        out.push_str(&serde_json::to_string(&json(m))?);
        out.push('\n');
    }
    Ok(out)
}

/// One JSONL message: the same shape as `bbs read --json`, plus `read_at`.
/// Every field is present, null when it does not apply; names are bare and
/// timestamps are the stored RFC3339 values.
#[derive(Serialize)]
struct MessageJson<'a> {
    id: i64,
    created_at: &'a str,
    from: &'a str,
    from_kind: &'static str,
    /// The sending agent's human; null for humans and unknown owners.
    from_owner: Option<&'a str>,
    /// Direct recipient; null for broadcasts.
    to: Option<&'a str>,
    /// Broadcast group; null for direct messages.
    group: Option<&'a str>,
    body: &'a str,
    /// When a direct message was read; null for broadcasts and unread mail.
    read_at: Option<&'a str>,
}

fn json(m: &Message) -> MessageJson<'_> {
    MessageJson {
        id: m.id,
        created_at: &m.created_at,
        from: &m.from_name,
        from_kind: match m.from_kind() {
            Kind::Agent => "agent",
            Kind::Human => "human",
            Kind::Unspecified => "unspecified",
        },
        from_owner: m.from_owner.as_deref(),
        to: m.to_name.as_deref(),
        group: m.to_group.as_deref(),
        body: &m.body,
        read_at: m.read_at.as_deref(),
    }
}

/// Plain text like `bbs read`, plus the sending agent's owner and read state.
fn format_feed(rows: &[Message]) -> String {
    let mut out = String::new();
    for (i, m) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let from = match (m.from_kind(), &m.from_owner) {
            (Kind::Human, _) => format!("{} (human)", m.from_name),
            (_, Some(owner)) => format!("{}@{owner}", m.from_name),
            (_, None) => m.from_name.clone(),
        };
        let to = match (&m.to_group, &m.to_name) {
            (Some(group), _) => format!("@{group}"),
            (None, to) => {
                let state = match &m.read_at {
                    Some(read) => format!("read {}", display_time(read)),
                    None => "unread".into(),
                };
                format!("{} ({state})", to.as_deref().unwrap_or("?"))
            }
        };
        out.push_str(&format!(
            "[{}] from {from} to {to}:\n",
            display_time(&m.created_at)
        ));
        out.push_str(m.body.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

async fn feed_loop(
    terminal: &mut DefaultTerminal,
    feed: &mut FeedWatch<'_>,
    mut rows: Vec<Message>,
) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| render(frame, &rows))?;
            dirty = false;
        }
        // Wait for the next snapshot, checking keys at least every KEY_POLL_INTERVAL.
        tokio::select! {
            snapshot = feed.next() => {
                rows = snapshot?;
                dirty = true;
            }
            _ = tokio::time::sleep(KEY_POLL_INTERVAL) => {}
        }
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c');
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(());
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame, rows: &[Message]) {
    let [title_area, feed_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(frame.area());

    let title = Line::from(vec![
        " bbs view ".bold().reversed(),
        format!("  {} messages  ·  q/Esc to quit", rows.len()).dark_gray(),
    ]);
    frame.render_widget(title, title_area);

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No messages yet. Waiting…".dark_gray()),
            feed_area,
        );
        return;
    }

    let mut text = Text::default();
    for row in rows {
        text.lines.push(Line::default());
        text.lines.push(header(row));
        for mut line in tui_markdown::from_str(&row.body).lines {
            line.spans.insert(0, Span::raw("  "));
            text.lines.push(line);
        }
    }

    // Follow the tail: scroll so the newest message sits at the bottom.
    let feed = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = feed.line_count(feed_area.width);
    let scroll = total.saturating_sub(feed_area.height as usize);
    let feed = feed.scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(feed, feed_area);
}

fn header(row: &Message) -> Line<'static> {
    let sent = local(&row.created_at);
    let mut spans = vec![
        Span::styled(
            sent.map_or_else(
                || row.created_at.clone(),
                |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        name_span(&row.sender_display_name(), row.from_kind()),
        Span::styled(" → ", Style::new().fg(Color::DarkGray)),
    ];
    match (&row.to_group, &row.to_name) {
        (Some(group), _) => spans.push(Span::styled(
            format!("@{group}"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, Some(to)) => {
            spans.push(Span::styled(
                to.clone(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("  "));
            spans.push(match row.read_at.as_deref().and_then(local) {
                Some(read) => {
                    let same_day = sent.is_some_and(|s| s.date_naive() == read.date_naive());
                    let fmt = if same_day {
                        "%H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M:%S"
                    };
                    Span::styled(
                        format!("read {}", read.format(fmt)),
                        Style::new().fg(Color::Green),
                    )
                }
                None => Span::styled("unread", Style::new().fg(Color::Red)),
            });
        }
        (None, None) => spans.push(Span::raw("?")),
    }
    Line::from(spans)
}

fn name_span(name: &str, kind: Kind) -> Span<'static> {
    let color = match kind {
        Kind::Human => Color::Magenta,
        Kind::Agent | Kind::Unspecified => Color::Cyan,
    };
    Span::styled(
        name.to_string(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn local(stored: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn row(group: Option<&str>, to: Option<&str>, read_at: Option<&str>, body: &str) -> Message {
        Message {
            id: 0,
            created_at: "2026-10-09T14:02:11.000000Z".into(),
            from_kind: Kind::Agent.into(),
            from_name: "Juliana".into(),
            from_owner: None,
            reader_owner: None,
            to_group: group.map(Into::into),
            to_name: to.map(Into::into),
            read_at: read_at.map(Into::into),
            body: body.into(),
        }
    }

    fn screen(rows: &[Message], height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        terminal.draw(|f| render(f, rows)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_headers_and_markdown() {
        let rows = [
            row(Some("acme"), None, None, "standup in **5 minutes**"),
            row(
                None,
                Some("Mateo"),
                Some("2026-10-09T14:03:00.000000Z"),
                "- one\n- two",
            ),
            row(None, Some("Aiko"), None, "ping"),
        ];
        let out = screen(&rows, 20);
        assert!(out.contains("3 messages"), "{out}");
        assert!(out.contains("Juliana → @acme"), "{out}");
        assert!(
            out.contains("standup in 5 minutes") && !out.contains("**"),
            "{out}"
        );
        assert!(out.contains("Juliana → Mateo  read "), "{out}");
        assert!(out.contains("Juliana → Aiko  unread"), "{out}");
        // Broadcasts never show a read time.
        let acme = out.lines().find(|l| l.contains("@acme")).unwrap();
        assert!(!acme.contains("read"), "{acme}");
    }

    #[test]
    fn follows_the_tail_when_overflowing() {
        let rows: Vec<_> = (0..30)
            .map(|i| row(Some("acme"), None, None, &format!("message number {i}")))
            .collect();
        let out = screen(&rows, 10);
        assert!(out.contains("message number 29"), "{out}");
        assert!(!out.contains("message number 0\n"), "{out}");
    }
}
