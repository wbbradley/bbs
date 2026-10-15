//! `bbs view`: a read-only feed of messages on the board, as an auto-following
//! TUI, plain text, or JSONL. It never archives anything, so watching does not
//! consume anyone's mail.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    /// The direct recipient agent's human; null otherwise and for unknown owners.
    to_owner: Option<&'a str>,
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
        to_owner: m.to_owner.as_deref(),
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
        let from = match m.from_kind() {
            Kind::Human => format!("{} (human)", m.from_name),
            _ => m.qualified_sender(),
        };
        let to = match (&m.to_group, m.qualified_recipient()) {
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
    rows: Vec<Message>,
) -> Result<()> {
    let mut view = ViewState::new(rows);
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| view.render(frame))?;
            dirty = false;
        }
        // Keep reading the stream whatever the view shows: pausing only freezes
        // the viewport, so the server never waits on this client. Check keys at
        // least every KEY_POLL_INTERVAL.
        tokio::select! {
            snapshot = feed.next() => {
                view.ingest(snapshot?);
                dirty = true;
            }
            _ = tokio::time::sleep(KEY_POLL_INTERVAL) => {}
        }
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if view.handle_key(key) {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

/// Loaded history kept while the view is open; the oldest rows are dropped
/// beyond this, never the stream.
const MAX_ROWS: usize = 10_000;

/// The TUI: every row loaded since start, and where the viewport sits in them.
struct ViewState {
    /// Oldest first, by message id. Snapshots append newer messages and
    /// refresh rows they still contain, so history only grows while open.
    rows: Vec<Message>,
    /// Wrapped line count of each row at a width, cached between frames.
    heights: Vec<Option<(u16, usize)>>,
    /// `None` follows the tail; otherwise the viewport's top line, anchored to
    /// a message so appends and dropped rows never move it.
    paused: Option<Anchor>,
    /// Messages appended while paused.
    new_count: usize,
    /// The search being typed after `/`.
    prompt: Option<String>,
    /// The committed search, lowercase.
    query: Option<String>,
    /// Line layout that keys scroll against.
    frame: Geometry,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Anchor {
    id: i64,
    line: usize,
}

/// Line layout of `rows` for the current terminal size; kept in step with
/// `rows`, so indexes into it are row indexes.
#[derive(Default)]
struct Geometry {
    width: u16,
    height: usize,
    /// First line of each row.
    starts: Vec<usize>,
    total: usize,
}

impl Geometry {
    fn bottom(&self) -> usize {
        self.total.saturating_sub(self.height)
    }
}

impl ViewState {
    fn new(rows: Vec<Message>) -> ViewState {
        let mut view = ViewState {
            rows: Vec::new(),
            heights: Vec::new(),
            paused: None,
            new_count: 0,
            prompt: None,
            query: None,
            frame: Geometry::default(),
        };
        view.ingest(rows);
        view
    }

    /// Merge a feed snapshot: refresh rows it still holds (e.g. `read_at`),
    /// append newer ones, and drop the oldest beyond MAX_ROWS.
    fn ingest(&mut self, snapshot: Vec<Message>) {
        for m in snapshot {
            match self.rows.binary_search_by_key(&m.id, |r| r.id) {
                Ok(i) => {
                    self.rows[i] = m;
                    self.heights[i] = None;
                }
                Err(i) if i == self.rows.len() => {
                    self.rows.push(m);
                    self.heights.push(None);
                    if self.paused.is_some() {
                        self.new_count += 1;
                    }
                }
                // Older than everything kept: it was dropped, or never loaded.
                Err(_) => {}
            }
        }
        let excess = self.rows.len().saturating_sub(MAX_ROWS);
        self.rows.drain(..excess);
        self.heights.drain(..excess);
        self.layout(self.frame.width, self.frame.height);
    }

    /// Recompute line starts for a feed area, measuring only uncached rows.
    fn layout(&mut self, width: u16, height: usize) {
        let mut starts = Vec::with_capacity(self.rows.len());
        let mut total = 0;
        for (row, cached) in self.rows.iter().zip(&mut self.heights) {
            starts.push(total);
            total += match *cached {
                Some((w, h)) if w == width => h,
                _ => {
                    let h = Paragraph::new(Text::from(message_lines(row, None)))
                        .wrap(Wrap { trim: false })
                        .line_count(width);
                    *cached = Some((width, h));
                    h
                }
            };
        }
        self.frame = Geometry {
            width,
            height,
            starts,
            total,
        };
    }

    /// The viewport's top line.
    fn top(&self) -> usize {
        let Some(anchor) = self.paused else {
            return self.frame.bottom();
        };
        let i = self.rows.partition_point(|r| r.id < anchor.id);
        let line = match self.rows.get(i) {
            Some(r) if r.id == anchor.id => anchor.line,
            _ => 0,
        };
        let start = self.frame.starts.get(i).copied().unwrap_or(0);
        (start + line).min(self.frame.bottom())
    }

    /// Pause with `line` at the top of the viewport.
    fn scroll_to(&mut self, line: usize) {
        let line = line.min(self.frame.bottom());
        let i = self
            .frame
            .starts
            .partition_point(|&s| s <= line)
            .saturating_sub(1);
        let Some(row) = self.rows.get(i) else {
            return;
        };
        if self.paused.is_none() {
            self.new_count = 0;
        }
        self.paused = Some(Anchor {
            id: row.id,
            line: line - self.frame.starts[i],
        });
    }

    fn follow(&mut self) {
        self.paused = None;
        self.new_count = 0;
    }

    fn matches(&self, row: &Message) -> bool {
        let Some(query) = &self.query else {
            return false;
        };
        [
            Some(row.qualified_sender()),
            row.qualified_recipient(),
            row.to_group.as_ref().map(|g| format!("@{g}")),
            Some(row.body.clone()),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(query.as_str()))
    }

    /// Jump to the next (or previous) matching message, wrapping around.
    fn jump(&mut self, forward: bool) {
        let top = self.top();
        let starts = &self.frame.starts;
        let hits: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.matches(&self.rows[i]))
            .collect();
        let target = if forward {
            hits.iter().find(|&&i| starts[i] > top).or(hits.first())
        } else {
            hits.iter()
                .rev()
                .find(|&&i| starts[i] < top)
                .or(hits.last())
        };
        if let Some(&i) = target {
            self.scroll_to(starts[i]);
        }
    }

    /// Apply one key press; `true` quits.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if let Some(prompt) = &mut self.prompt {
            match key.code {
                KeyCode::Enter => {
                    let query = std::mem::take(prompt).to_lowercase();
                    self.prompt = None;
                    self.query = (!query.is_empty()).then_some(query);
                    // From the tail, the newest match is the nearest one.
                    let forward = self.paused.is_some();
                    self.jump(forward);
                }
                KeyCode::Esc => self.prompt = None,
                KeyCode::Backspace => {
                    prompt.pop();
                }
                KeyCode::Char(c) => prompt.push(c),
                _ => {}
            }
            return false;
        }
        let top = self.top();
        let page = self.frame.height.max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_to(top.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_to(top + 1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_to(top.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_to(top + page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_to(0),
            KeyCode::End | KeyCode::Char('G' | 'F') => self.follow(),
            KeyCode::Char('/') => self.prompt = Some(String::new()),
            KeyCode::Char('n') => self.jump(true),
            KeyCode::Char('N') => self.jump(false),
            _ => {}
        }
        false
    }

    fn render(&mut self, frame: &mut Frame) {
        let [title_area, feed_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(frame.area());
        self.layout(feed_area.width, feed_area.height as usize);
        let top = self.top();

        frame.render_widget(self.title(), title_area);
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No messages yet. Waiting…".dark_gray()),
                feed_area,
            );
            return;
        }

        // Lay out only the rows from the one holding the top line, so scroll
        // offsets stay small however much history is loaded.
        let first = self
            .frame
            .starts
            .partition_point(|&s| s <= top)
            .saturating_sub(1);
        let mut text = Text::default();
        for (i, row) in self.rows.iter().enumerate().skip(first) {
            text.lines.extend(message_lines(row, self.query.as_deref()));
            let end = self.frame.starts.get(i + 1).unwrap_or(&self.frame.total);
            if *end >= top + self.frame.height {
                break;
            }
        }
        let offset = top - self.frame.starts[first];
        let feed = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(feed, feed_area);
    }

    fn title(&self) -> Line<'static> {
        let mut spans = vec![" bbs view ".bold().reversed()];
        if let Some(prompt) = &self.prompt {
            spans.push(format!("  /{prompt}").into());
            spans.push("█".dark_gray());
            spans.push("  Enter search · Esc cancel".dark_gray());
            return Line::from(spans);
        }
        let state = match self.paused {
            None => "following".to_string(),
            Some(_) if self.new_count > 0 => format!("paused · {} new · G follows", self.new_count),
            Some(_) => "paused · G follows".to_string(),
        };
        spans.push(format!("  {} messages · {state}", self.rows.len()).dark_gray());
        if let Some(query) = &self.query {
            let hits = self.rows.iter().filter(|r| self.matches(r)).count();
            spans.push(format!(" · /{query}: {hits} matches · n/N").dark_gray());
        }
        spans.push("  ↑↓ PgUp/PgDn g/G scroll · / search · q quit".dark_gray());
        Line::from(spans)
    }
}

/// A message as displayed: a blank separator, the header, and the indented
/// markdown body, with `query` matches highlighted.
fn message_lines<'a>(row: &'a Message, query: Option<&str>) -> Vec<Line<'a>> {
    let mut lines = vec![Line::default(), header(row)];
    for mut line in tui_markdown::from_str(&row.body).lines {
        line.spans.insert(0, Span::raw("  "));
        lines.push(line);
    }
    match query {
        Some(query) => lines.into_iter().map(|l| highlight(l, query)).collect(),
        None => lines,
    }
}

/// Reverse-video every case-insensitive occurrence of `query` within a span.
fn highlight<'a>(line: Line<'a>, query: &str) -> Line<'a> {
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let lower = span.content.to_lowercase();
        // Lowercasing can change byte offsets outside ASCII; leave such spans as is.
        if lower.len() != span.content.len() || !lower.contains(query) {
            spans.push(span);
            continue;
        }
        let content = span.content.to_string();
        let mut at = 0;
        for (i, _) in lower.match_indices(query) {
            if i > at {
                spans.push(Span::styled(content[at..i].to_string(), span.style));
            }
            let end = i + query.len();
            spans.push(Span::styled(
                content[i..end].to_string(),
                span.style.add_modifier(Modifier::REVERSED),
            ));
            at = end;
        }
        if at < content.len() {
            spans.push(Span::styled(content[at..].to_string(), span.style));
        }
    }
    Line { spans, ..line }
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
        name_span(&row.qualified_sender(), row.from_kind()),
        Span::styled(" → ", Style::new().fg(Color::DarkGray)),
    ];
    match (&row.to_group, row.qualified_recipient()) {
        (Some(group), _) => spans.push(Span::styled(
            format!("@{group}"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, Some(to)) => {
            spans.push(Span::styled(to, Style::new().add_modifier(Modifier::BOLD)));
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
            to_owner: None,
            read_at: read_at.map(Into::into),
            body: body.into(),
        }
    }

    /// A view of `rows`, numbering them 1.. in order.
    fn view(rows: &[Message]) -> ViewState {
        let rows = rows
            .iter()
            .zip(1..)
            .map(|(r, id)| Message { id, ..r.clone() })
            .collect();
        ViewState::new(rows)
    }

    fn screen(rows: &[Message], height: u16) -> String {
        draw(&mut view(rows), height)
    }

    fn draw(view: &mut ViewState, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        terminal.draw(|f| view.render(f)).unwrap();
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
    fn headers_qualify_agents_with_their_humans() {
        let mut dm = row(None, Some("Mateo"), None, "hi");
        dm.from_owner = Some("will".into());
        dm.to_owner = Some("ada".into());
        let mut from_human = row(Some("acme"), None, None, "hello");
        from_human.from_kind = Kind::Human.into();
        from_human.from_name = "ada".into();
        let out = screen(&[dm, from_human], 20);
        assert!(out.contains("Juliana@will → Mateo@ada  unread"), "{out}");
        assert!(out.contains("ada → @acme"), "{out}");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn numbered(range: std::ops::Range<i64>) -> Vec<Message> {
        range
            .map(|i| Message {
                id: i + 1,
                ..row(Some("acme"), None, None, &format!("message number {i}"))
            })
            .collect()
    }

    /// The feed lines below the title.
    fn feed(screen: &str) -> Vec<&str> {
        screen.lines().skip(1).collect()
    }

    #[test]
    fn paused_view_keeps_ingesting_without_moving() {
        let mut view = ViewState::new(numbered(0..30));
        assert!(draw(&mut view, 10).contains("message number 29"));
        view.handle_key(key(KeyCode::Char('k')));
        let paused = draw(&mut view, 10);
        assert!(paused.contains("paused"), "{paused}");
        // The stream is still consumed while paused; only the viewport is frozen.
        for i in 30..40 {
            view.ingest(numbered(i..i + 1));
        }
        assert_eq!(view.rows.len(), 40);
        let later = draw(&mut view, 10);
        assert_eq!(feed(&later), feed(&paused));
        assert!(later.contains("10 new"), "{later}");
        view.handle_key(key(KeyCode::Char('G')));
        let following = draw(&mut view, 10);
        assert!(following.contains("message number 39"), "{following}");
        assert!(following.contains("following") && !following.contains("new"));
    }

    #[test]
    fn scroll_keys_move_by_line_page_and_end() {
        let mut view = ViewState::new(numbered(0..30));
        draw(&mut view, 10);
        let bottom = view.top();
        view.handle_key(key(KeyCode::PageUp));
        assert_eq!(view.top(), bottom - 9);
        view.handle_key(key(KeyCode::Char('j')));
        assert_eq!(view.top(), bottom - 8);
        view.handle_key(key(KeyCode::Char('g')));
        assert_eq!(view.top(), 0);
        assert!(draw(&mut view, 10).contains("message number 0"));
        view.handle_key(key(KeyCode::Up));
        assert_eq!(view.top(), 0);
        view.handle_key(key(KeyCode::PageDown));
        assert_eq!(view.top(), 9);
        view.handle_key(key(KeyCode::End));
        assert_eq!(view.paused, None);
        assert_eq!(view.top(), bottom);
    }

    #[test]
    fn search_jumps_between_matches_and_highlights() {
        let mut view = ViewState::new(numbered(0..30));
        draw(&mut view, 10);
        for code in [KeyCode::Char('/')]
            .into_iter()
            .chain("NUMBER 1".chars().map(KeyCode::Char))
        {
            view.handle_key(key(code));
        }
        assert!(draw(&mut view, 10).contains("/NUMBER 1"));
        view.handle_key(key(KeyCode::Enter));
        // From the tail, the newest match comes first; n and N wrap around.
        let out = draw(&mut view, 10);
        assert!(feed(&out)[2].ends_with("message number 19"), "{out}");
        assert!(out.contains("11 matches"), "{out}");
        view.handle_key(key(KeyCode::Char('n')));
        assert!(feed(&draw(&mut view, 10))[2].ends_with("message number 1"));
        view.handle_key(key(KeyCode::Char('n')));
        assert!(feed(&draw(&mut view, 10))[2].ends_with("message number 10"));
        view.handle_key(key(KeyCode::Char('N')));
        view.handle_key(key(KeyCode::Char('N')));
        assert!(feed(&draw(&mut view, 10))[2].ends_with("message number 19"));

        let line = highlight(Line::from("a Needle, a needle"), "needle");
        let marked: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, ["Needle", "needle"]);
    }

    #[test]
    fn prompt_escape_cancels_instead_of_quitting() {
        let mut view = ViewState::new(numbered(0..3));
        assert!(!view.handle_key(key(KeyCode::Char('/'))));
        assert!(!view.handle_key(key(KeyCode::Char('q'))));
        assert!(!view.handle_key(key(KeyCode::Esc)));
        assert_eq!(
            (view.prompt.as_deref(), view.query.as_deref()),
            (None, None)
        );
        view.handle_key(key(KeyCode::Char('/')));
        assert!(view.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(ViewState::new(Vec::new()).handle_key(key(KeyCode::Esc)));
    }

    #[test]
    fn snapshots_refresh_rows_and_the_oldest_are_dropped_past_the_cap() {
        let mut view = ViewState::new(numbered(0..3));
        let mut read = numbered(1..2);
        read[0].read_at = Some("2026-10-09T14:03:00.000000Z".into());
        // A later window that no longer holds the first row keeps it loaded.
        view.ingest(read);
        assert_eq!(view.rows.len(), 3);
        assert!(view.rows[1].read_at.is_some());

        draw(&mut view, 10);
        view.handle_key(key(KeyCode::Char('g')));
        view.ingest(numbered(3..MAX_ROWS as i64 + 5));
        assert_eq!(view.rows.len(), MAX_ROWS);
        assert_eq!(view.rows[0].id, 6);
        // The anchored row was dropped, so the view holds at the oldest kept row.
        assert_eq!(view.top(), 0);
        assert!(draw(&mut view, 10).contains("message number 5"));
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
