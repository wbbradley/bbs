//! `bbs view`: a read-only feed of messages on the board, as an auto-following
//! TUI, plain text, or JSONL. It never archives anything, so watching does not
//! consume anyone's mail.

use std::collections::HashSet;
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
    let window = req.backlog as usize;
    let mut feed = backend.watch_feed(req, false).await?;
    // Fetch before taking over the terminal, so a refusal prints normally.
    let rows = feed.next().await?;
    if !follow {
        write_out(&format_feed(&rows))?;
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = feed_loop(&mut terminal, &mut feed, rows, window).await;
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
    window: usize,
) -> Result<()> {
    let mut view = ViewState::new(rows, window);
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
    /// a message so appends never move it. If the cap drops the anchored row,
    /// the view holds at the oldest kept row.
    paused: Option<Anchor>,
    /// Messages appended while paused.
    new_count: usize,
    /// The search being typed after `/`.
    prompt: Option<String>,
    /// The committed search, case-folded by `fold`.
    query: Option<String>,
    /// Whether each row matches `query`, kept in step with `rows`.
    matched: Vec<bool>,
    /// The server's window: each snapshot holds at most this many newest
    /// matching messages.
    window: usize,
    /// Rows that messages may be missing before: a full snapshot arrived whose
    /// oldest message was newer than every row held, so a burst outran it.
    gaps: HashSet<i64>,
    /// The first snapshot has arrived; later ones can reveal gaps.
    started: bool,
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
    fn new(rows: Vec<Message>, window: usize) -> ViewState {
        let mut view = ViewState {
            rows: Vec::new(),
            heights: Vec::new(),
            paused: None,
            new_count: 0,
            prompt: None,
            query: None,
            matched: Vec::new(),
            window,
            gaps: HashSet::new(),
            started: false,
            frame: Geometry::default(),
        };
        view.ingest(rows);
        view
    }

    /// Merge a feed snapshot: refresh rows it still holds (e.g. `read_at`),
    /// append newer ones, and drop the oldest beyond MAX_ROWS.
    fn ingest(&mut self, snapshot: Vec<Message>) {
        // A full window entirely newer than what is held may have skipped
        // messages; exactly --limit new ones look the same, so this can be a
        // false positive. A one-message window is always full, so it is not
        // checked.
        let newest = self.rows.last().map_or(0, |r| r.id);
        if let Some(first) = snapshot.first()
            && self.started
            && self.window > 1
            && snapshot.len() >= self.window
            && first.id > newest
        {
            self.gaps.insert(first.id);
        }
        self.started = true;
        for m in snapshot {
            match self.rows.binary_search_by_key(&m.id, |r| r.id) {
                Ok(i) if self.rows[i] != m => {
                    self.matched[i] = self.matches(&m);
                    self.rows[i] = m;
                    self.heights[i] = None;
                }
                Ok(_) => {}
                Err(i) if i == self.rows.len() => {
                    self.matched.push(self.matches(&m));
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
        self.matched.drain(..excess);
        if let Some(oldest) = self.rows.first() {
            self.gaps.retain(|&id| id >= oldest.id);
        }
        self.layout(self.frame.width, self.frame.height);
    }

    /// Recompute line starts for a feed area, measuring only uncached rows.
    fn layout(&mut self, width: u16, height: usize) {
        let mut starts = Vec::with_capacity(self.rows.len());
        let mut total = 0;
        for (row, cached) in self.rows.iter().zip(&mut self.heights) {
            starts.push(total);
            // Nothing is drawn before the first frame sizes the feed area.
            if width == 0 {
                continue;
            }
            total += match *cached {
                Some((w, h)) if w == width => h,
                _ => {
                    let gap = self.gaps.contains(&row.id);
                    let h = Paragraph::new(Text::from(message_lines(row, gap, None)))
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
        let start = self.frame.starts.get(i).copied().unwrap_or(0);
        let line = match self.rows.get(i) {
            // A resize can shorten the row the anchor line was measured in.
            Some(r) if r.id == anchor.id => {
                let end = self.frame.starts.get(i + 1).unwrap_or(&self.frame.total);
                anchor.line.min(end.saturating_sub(start + 1))
            }
            _ => 0,
        };
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

    /// Scroll down to `line`; following stays on when there is nothing below.
    fn scroll_down(&mut self, line: usize) {
        if self.paused.is_some() || line < self.frame.bottom() {
            self.scroll_to(line);
        }
    }

    fn follow(&mut self) {
        self.paused = None;
        self.new_count = 0;
    }

    /// Whether the search matches the row's sender, recipient, group, or body
    /// as sent (markdown included).
    fn matches(&self, row: &Message) -> bool {
        let Some(query) = &self.query else {
            return false;
        };
        [
            Some(row.qualified_sender()),
            row.qualified_recipient(),
            row.to_group.as_ref().map(|g| format!("@{g}")),
        ]
        .iter()
        .flatten()
        .map(String::as_str)
        .chain([row.body.as_str()])
        .any(|text| !find_matches(text, query).is_empty())
    }

    fn set_query(&mut self, query: &str) {
        let query: String = query.chars().map(fold).collect();
        self.query = (!query.is_empty()).then_some(query);
        self.matched = self.rows.iter().map(|r| self.matches(r)).collect();
    }

    /// Jump to the next (or previous) matching message, wrapping around. A
    /// match already in view at the tail counts as passed, so `n` moves on.
    fn jump(&mut self, forward: bool) {
        let top = self.top();
        let bottom = self.frame.bottom();
        let starts = &self.frame.starts;
        let mut hits = (0..self.rows.len()).filter(|&i| self.matched[i]);
        let target = if forward {
            let first = hits.clone().next();
            hits.find(|&i| starts[i].min(bottom) > top).or(first)
        } else {
            let last = hits.clone().next_back();
            hits.rev().find(|&i| starts[i] < top).or(last)
        };
        if let Some(i) = target {
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
                    let query = std::mem::take(prompt);
                    self.prompt = None;
                    self.set_query(&query);
                    // From the tail, the newest match is the nearest one.
                    if self.paused.is_some() {
                        self.jump(true);
                    } else if let Some(i) = self.matched.iter().rposition(|&m| m) {
                        self.scroll_to(self.frame.starts[i]);
                    }
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
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(top + 1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_to(top.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(top + page),
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
            let gap = self.gaps.contains(&row.id);
            text.lines
                .extend(message_lines(row, gap, self.query.as_deref()));
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
            let hits = self.matched.iter().filter(|&&m| m).count();
            spans.push(format!(" · /{query}: {hits} matches · n/N").dark_gray());
        }
        spans.push("  ↑↓ PgUp/PgDn g/G scroll · / search · q quit".dark_gray());
        Line::from(spans)
    }
}

/// Header spans that hold searchable text: the sender and the recipient or group.
const HEADER_NAMES: [usize; 2] = [2, 4];

/// A message as displayed: a blank separator, a marker when messages may be
/// missing before it, the header, and the indented markdown body, with
/// `query` matches highlighted in the names and body.
fn message_lines<'a>(row: &'a Message, gap: bool, query: Option<&str>) -> Vec<Line<'a>> {
    let mut header = header(row);
    let mut body = Vec::new();
    for mut line in tui_markdown::from_str(&row.body).lines {
        line.spans.insert(0, Span::raw("  "));
        body.push(line);
    }
    if let Some(query) = query {
        header.spans = header
            .spans
            .into_iter()
            .enumerate()
            .flat_map(|(i, span)| match HEADER_NAMES.contains(&i) {
                true => highlight(span, query),
                false => vec![span],
            })
            .collect();
        for line in &mut body {
            line.spans = std::mem::take(&mut line.spans)
                .into_iter()
                .flat_map(|span| highlight(span, query))
                .collect();
        }
    }
    let marker = gap.then(|| {
        Line::from(
            "  ⋯ messages may be missing here: more than --limit arrived at once; use --since/--until or a larger --limit"
                .yellow(),
        )
    });
    [Line::default()]
        .into_iter()
        .chain(marker)
        .chain([header])
        .chain(body)
        .collect()
}

/// Case folding for search: a character's lowercase form when that is one
/// character, so folded text keeps a character-for-character correspondence.
fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// Byte ranges of non-overlapping case-insensitive occurrences of the folded
/// `query`, compared character by character so ranges always fall on
/// character boundaries of `text`.
fn find_matches(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let query: Vec<char> = query.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while !query.is_empty() && i + query.len() <= chars.len() {
        let hit = chars[i..i + query.len()]
            .iter()
            .zip(&query)
            .all(|(&(_, c), &q)| fold(c) == q);
        if hit {
            // Keep combining marks with the character they decorate.
            let mut next = i + query.len();
            while chars.get(next).is_some_and(|&(_, c)| is_combining(c)) {
                next += 1;
            }
            let end = chars.get(next).map_or(text.len(), |&(b, _)| b);
            found.push(chars[i].0..end);
            i = next;
        } else {
            i += 1;
        }
    }
    found
}

/// Common combining diacritical mark blocks.
fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Split a span so every `query` match is in reverse video.
fn highlight<'a>(span: Span<'a>, query: &str) -> Vec<Span<'a>> {
    let found = find_matches(&span.content, query);
    if found.is_empty() {
        return vec![span];
    }
    let content = span.content.to_string();
    let mut spans = Vec::new();
    let mut at = 0;
    for range in found {
        if range.start > at {
            spans.push(Span::styled(
                content[at..range.start].to_string(),
                span.style,
            ));
        }
        spans.push(Span::styled(
            content[range.clone()].to_string(),
            span.style.add_modifier(Modifier::REVERSED),
        ));
        at = range.end;
    }
    if at < content.len() {
        spans.push(Span::styled(content[at..].to_string(), span.style));
    }
    spans
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
        ViewState::new(rows, BACKLOG as usize)
    }

    fn screen(rows: &[Message], height: u16) -> String {
        draw(&mut view(rows), height)
    }

    fn draw(view: &mut ViewState, height: u16) -> String {
        draw_sized(view, 70, height)
    }

    fn draw_sized(view: &mut ViewState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
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
        let mut view = ViewState::new(numbered(0..30), 1000);
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
        let mut view = ViewState::new(numbered(0..30), 1000);
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
        let mut view = ViewState::new(numbered(0..30), 1000);
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

        let spans = highlight(Span::raw("a Needle, a needle"), "needle");
        let marked: Vec<_> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, ["Needle", "needle"]);
    }

    #[test]
    fn prompt_escape_cancels_instead_of_quitting() {
        let mut view = ViewState::new(numbered(0..3), 1000);
        assert!(!view.handle_key(key(KeyCode::Char('/'))));
        assert!(!view.handle_key(key(KeyCode::Char('q'))));
        assert!(!view.handle_key(key(KeyCode::Esc)));
        assert_eq!(
            (view.prompt.as_deref(), view.query.as_deref()),
            (None, None)
        );
        view.handle_key(key(KeyCode::Char('/')));
        assert!(view.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(ViewState::new(Vec::new(), 1000).handle_key(key(KeyCode::Esc)));
    }

    #[test]
    fn snapshots_refresh_rows_and_the_oldest_are_dropped_past_the_cap() {
        let mut view = ViewState::new(numbered(0..3), 1000);
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

    /// 30 rows (3 lines each), with `hits` containing "needle".
    fn with_needles(hits: &[i64]) -> ViewState {
        let mut rows = numbered(0..30);
        for &i in hits {
            rows[i as usize].body.push_str(" needle");
        }
        ViewState::new(rows, 1000)
    }

    fn search(view: &mut ViewState, query: &str) {
        view.handle_key(key(KeyCode::Char('/')));
        for c in query.chars() {
            view.handle_key(key(KeyCode::Char(c)));
        }
        view.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn highlight_handles_case_folding_that_changes_byte_lengths() {
        // Ⱥ grows and Ω (OHM SIGN) shrinks when lowercased.
        let spans = highlight(Span::raw("ȺΩ"), "ω");
        let marked: Vec<_> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, ["Ω"]);
        let mut view = view(&[row(Some("acme"), None, None, "ȺΩ")]);
        draw(&mut view, 10);
        search(&mut view, "ω");
        assert!(draw(&mut view, 10).contains("1 matches"));
    }

    #[test]
    fn next_match_moves_on_from_matches_at_the_tail() {
        let mut view = with_needles(&[26, 28]);
        draw(&mut view, 10);
        view.handle_key(key(KeyCode::Char('g')));
        search(&mut view, "needle");
        let mut tops = vec![view.top()];
        for _ in 0..4 {
            view.handle_key(key(KeyCode::Char('n')));
            tops.push(view.top());
        }
        // Row 28 can only scroll to the bottom; the next `n` wraps to row 26.
        assert_eq!(tops, [78, 81, 78, 81, 78]);
    }

    #[test]
    fn search_from_the_tail_starts_at_the_newest_match() {
        let mut view = with_needles(&[20, 29]);
        draw(&mut view, 10);
        search(&mut view, "needle");
        assert_eq!(view.top(), view.frame.bottom());
        assert!(view.paused.is_some());
        view.handle_key(key(KeyCode::Char('N')));
        assert_eq!(view.top(), 60);
    }

    #[test]
    fn keys_below_the_tail_keep_following() {
        let mut view = ViewState::new(numbered(0..30), 1000);
        draw(&mut view, 10);
        for code in [
            KeyCode::Char('j'),
            KeyCode::Down,
            KeyCode::PageDown,
            KeyCode::Char(' '),
        ] {
            view.handle_key(key(code));
            assert_eq!(view.paused, None, "{code:?}");
        }
    }

    #[test]
    fn anchor_stays_in_its_row_after_a_resize() {
        let long = "word ".repeat(40);
        let mut view = view(&[
            row(Some("acme"), None, None, &long),
            row(Some("acme"), None, None, "after"),
        ]);
        draw_sized(&mut view, 20, 5);
        view.handle_key(key(KeyCode::Char('g')));
        view.scroll_to(9);
        let anchor = view.paused.unwrap();
        assert_eq!((anchor.id, anchor.line), (1, 9));
        draw_sized(&mut view, 400, 4);
        // The row is now 3 lines; the anchor clamps to its last line.
        assert_eq!(view.top(), 2.min(view.frame.bottom()));
    }

    #[test]
    fn rows_are_measured_only_once_the_feed_area_is_known() {
        let mut view = ViewState::new(numbered(0..30), 1000);
        assert!(view.heights.iter().all(Option::is_none));
        draw(&mut view, 10);
        assert!(view.heights.iter().all(Option::is_some));
    }

    #[test]
    fn gap_detection_covers_an_empty_start_and_skips_one_message_windows() {
        let mut view = ViewState::new(Vec::new(), 3);
        view.ingest(numbered(10..13));
        assert_eq!(view.gaps, HashSet::from([11]));
        let mut view = ViewState::new(numbered(0..1), 1);
        for i in 1..5 {
            view.ingest(numbered(i..i + 1));
        }
        assert!(view.gaps.is_empty());
    }

    #[test]
    fn a_marker_on_the_oldest_kept_row_stays_drawn() {
        let mut view = ViewState::new(numbered(0..3), 3);
        view.ingest(numbered(10..13));
        // Three past the cap: rows 1-3 are dropped.
        view.ingest(numbered(13..MAX_ROWS as i64 + 10));
        // Row 11 (the first after the gap) is now the oldest kept row.
        assert_eq!(view.rows[0].id, 11);
        assert!(view.gaps.contains(&11));
        draw(&mut view, 10);
        view.handle_key(key(KeyCode::Char('g')));
        let out = draw(&mut view, 10);
        assert!(out.contains("messages may be missing"), "{out}");
        assert!(
            out.contains("message number 10") && out.contains("message number 11"),
            "{out}"
        );
    }

    #[test]
    fn unchanged_rows_keep_their_cached_layout() {
        let mut view = ViewState::new(numbered(0..3), 1000);
        draw(&mut view, 10);
        view.ingest(numbered(0..3));
        assert!(view.heights.iter().all(Option::is_some));
    }

    #[test]
    fn highlight_keeps_combining_marks_with_their_letter() {
        let spans = highlight(Span::raw("e\u{301}cole"), "e");
        assert_eq!(spans[0].content, "e\u{301}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "e\u{301}cole");
    }

    #[test]
    fn a_full_snapshot_past_the_held_rows_marks_a_gap() {
        let mut view = ViewState::new(numbered(0..3), 3);
        // A partial snapshot holds every matching message, so nothing is missing.
        view.ingest(numbered(3..5));
        // Overlapping the held rows, a full snapshot misses nothing either.
        view.ingest(numbered(3..6));
        assert!(view.gaps.is_empty());
        // A full snapshot entirely past the held rows may have skipped some.
        view.ingest(numbered(10..13));
        assert_eq!(view.gaps, HashSet::from([11]));
        let out = draw(&mut view, 40);
        let marker = out
            .lines()
            .position(|l| l.contains("messages may be missing"))
            .unwrap();
        let next = out
            .lines()
            .position(|l| l.contains("message number 10"))
            .unwrap();
        let before = out
            .lines()
            .position(|l| l.contains("message number 5"))
            .unwrap();
        assert!(before < marker && marker < next, "{out}");
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
