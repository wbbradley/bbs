//! `bbs view`: a read-only feed of messages on the board, as an auto-following
//! TUI, plain text, or JSONL. It never archives anything, so watching does not
//! consume anyone's mail.

use crate::ui::theme::{Role, style};
use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Styled;
use ratatui::text::Line;
use ratatui::{DefaultTerminal, Frame};
use serde::Serialize;

use crate::app::{display_time, parse_time};
use crate::backend::{Backend, FeedWatch};
use crate::cli::ViewArgs;
use crate::proto::{FeedRequest, Kind, Message};
use crate::ui::card::Card;
use crate::ui::thread::ThreadView;

const BACKLOG: u32 = 200;
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long the screen waits to learn who is viewing before drawing anyway.
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(2);

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
    // Only the screen needs to know whose mail is whose; a human's own sits
    // on their surface. An agent, or no answer in time, sees only others'.
    let viewer = if tui {
        tokio::time::timeout(WHOAMI_TIMEOUT, backend.whoami())
            .await
            .ok()
            .and_then(Result::ok)
            .filter(|p| p.kind() == Kind::Human)
            .map(|p| p.name)
    } else {
        None
    };
    let mut feed = backend.watch_feed(req, false).await?;
    // Fetch before taking over the terminal, so a refusal prints normally.
    let rows = feed.next().await?;
    if !follow {
        write_out(&format_feed(&rows))?;
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = feed_loop(&mut terminal, &mut feed, rows, window, viewer).await;
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
        with: args.with.clone(),
        include_group_deliveries: false,
        sync: false,
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
    viewer: Option<String>,
) -> Result<()> {
    let mut view = ViewState::new(rows, window, viewer);
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

/// The TUI: a thread of message cards, with the search prompt being typed.
struct ViewState {
    thread: ThreadView<Card>,
    /// The human viewing, whose own mail is drawn as theirs.
    viewer: Option<String>,
    /// The search being typed after `/`.
    prompt: Option<String>,
}

impl std::ops::Deref for ViewState {
    type Target = ThreadView<Card>;
    fn deref(&self) -> &ThreadView<Card> {
        &self.thread
    }
}

impl std::ops::DerefMut for ViewState {
    fn deref_mut(&mut self) -> &mut ThreadView<Card> {
        &mut self.thread
    }
}

impl ViewState {
    fn new(rows: Vec<Message>, window: usize, viewer: Option<String>) -> ViewState {
        let rows = rows
            .into_iter()
            .map(|m| Card::feed(m, viewer.as_deref()))
            .collect();
        ViewState {
            thread: ThreadView::new(rows, window),
            viewer,
            prompt: None,
        }
    }

    fn ingest(&mut self, snapshot: Vec<Message>) {
        let rows = snapshot
            .into_iter()
            .map(|m| Card::feed(m, self.viewer.as_deref()))
            .collect();
        self.thread.ingest(rows);
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
                    self.search(&query);
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
        frame.render_widget(
            ratatui::widgets::Block::new().style(style(Role::Screen)),
            frame.area(),
        );
        let [title_area, feed_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(frame.area());
        // Lay out first, so the title reflects this frame.
        self.thread
            .layout(feed_area.width, feed_area.height as usize);
        frame.render_widget(self.title(), title_area);
        self.thread.render(frame, feed_area);
    }

    fn title(&self) -> Line<'static> {
        let mut spans = vec![" bbs view ".set_style(style(Role::TitleBar))];
        if let Some(prompt) = &self.prompt {
            spans.push(format!("  /{prompt}").into());
            spans.push("█".set_style(style(Role::Muted)));
            spans.push("  Enter search · Esc cancel".set_style(style(Role::Muted)));
            return Line::from(spans);
        }
        let state = match self.paused {
            None => "following".to_string(),
            Some(_) if self.new_count > 0 => format!("paused · {} new · G follows", self.new_count),
            Some(_) => "paused · G follows".to_string(),
        };
        spans.push(
            format!("  {} messages · {state}", self.rows.len()).set_style(style(Role::Muted)),
        );
        if let Some((query, hits)) = self.search_status() {
            spans.push(format!(" · /{query}: {hits} matches · n/N").set_style(style(Role::Muted)));
        }
        spans.push("  ↑↓ PgUp/PgDn g/G scroll · / search · q quit".set_style(style(Role::Muted)));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::message::highlight;
    use crate::ui::thread::MAX_ROWS;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::text::Span;
    use std::collections::HashSet;

    #[test]
    fn standalone_feed_uses_each_scheme() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                renders_cards_with_their_time_and_unread_direct_mail();
                search_jumps_between_matches_and_highlights();
                prompt_escape_cancels_instead_of_quitting();
            });
        }
    }

    fn row(group: Option<&str>, to: Option<&str>, read_at: Option<&str>, body: &str) -> Message {
        Message {
            id: 0,
            created_at: "2026-10-09T14:02:11.000000Z".into(),
            from_kind: Kind::Agent.into(),
            from_name: "Juliana".into(),
            from_owner: None,
            reader_owner: None,
            auto_reply: None,
            to_group: group.map(Into::into),
            to_name: to.map(Into::into),
            to_owner: None,
            read_at: read_at.map(Into::into),
            body: body.into(),
            control: None,
            timer_id: None,
        }
    }

    /// A view of `rows`, numbering them 1.. in order.
    fn view(rows: &[Message]) -> ViewState {
        let rows = rows
            .iter()
            .zip(1..)
            .map(|(r, id)| Message { id, ..r.clone() })
            .collect();
        ViewState::new(rows, BACKLOG as usize, None)
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
        crate::ui::theme::testing::assert_buffer(buffer);
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
    fn renders_cards_with_their_time_and_unread_direct_mail() {
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
        let out = screen(&rows, 24);
        assert!(out.contains("3 messages"), "{out}");
        assert!(out.contains("• Juliana → @acme"), "{out}");
        assert!(
            out.contains("standup in 5 minutes") && !out.contains("**"),
            "{out}"
        );
        // Every header carries its local send time at the right.
        let sent = chrono::DateTime::parse_from_rfc3339(&rows[0].created_at)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %-I:%M:%S%P")
            .to_string();
        for header in ["Juliana → @acme", "Juliana → Mateo", "Juliana → Aiko"] {
            let line = out.lines().find(|l| l.contains(header)).expect(header);
            assert!(line.ends_with(&sent), "{line}");
        }
        // Only unread direct mail says so; no read times.
        assert_eq!(out.matches("unread").count(), 1, "{out}");
        let ping = out.lines().position(|l| l.contains("ping")).unwrap();
        assert!(
            out.lines()
                .nth(ping + 1)
                .unwrap()
                .trim_end()
                .ends_with("unread"),
            "{out}"
        );
        assert!(
            !out.contains("read 2026") && !out.contains(" read "),
            "{out}"
        );
    }

    #[test]
    fn names_are_bare_and_the_viewers_own_mail_is_theirs() {
        use crate::ui::theme::{self, Role};
        let mut dm = row(None, Some("Mateo"), None, "hi");
        dm.from_owner = Some("will".into());
        dm.to_owner = Some("ada".into());
        let mut from_human = row(None, Some("Mateo"), None, "mine");
        from_human.from_kind = Kind::Human.into();
        from_human.from_name = "ada".into();
        from_human.to_owner = Some("will".into());
        let rows: Vec<Message> = [dm, from_human]
            .into_iter()
            .zip(1..)
            .map(|(r, id)| Message { id, ..r })
            .collect();
        let mut view = ViewState::new(rows, BACKLOG as usize, Some("ada".into()));
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal.draw(|f| view.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        crate::ui::theme::testing::assert_buffer(buffer);
        let lines: Vec<String> = (0..20)
            .map(|y| (0..70).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let out = lines.join("\n");
        assert!(out.contains("• Juliana → Mateo"), "{out}");
        assert!(out.contains("• ada → Mateo"), "{out}");
        assert!(!out.contains("@will") && !out.contains("@ada"), "{out}");
        let at = |text: &str| lines.iter().position(|l| l.contains(text)).unwrap() as u16;
        let bg = |role| theme::style(Role::Screen).patch(theme::style(role)).bg;
        assert_eq!(
            Some(buffer[(69, at("mine"))].bg),
            bg(Role::HumanSurface),
            "the viewer's own"
        );
        assert_eq!(
            Some(buffer[(69, at("hi"))].bg),
            bg(Role::OtherConversation),
            "someone else's"
        );
    }

    #[test]
    fn cards_fit_every_width_and_scheme_and_a_narrow_header_puts_the_time_above() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        let mut long = row(None, Some("Mateo"), None, "x");
        long.from_name = "AVeryLongAgentName".into();
        long.to_name = Some("AnotherLongAgentName".into());
        let rows = [
            row(Some("acme"), None, None, "group words"),
            row(None, Some("Mateo"), None, "direct words"),
            long,
        ];
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for width in [60u16, 80, 120] {
                    let out = draw_sized(&mut view(&rows), width, 24);
                    assert!(out.contains("• Juliana → @acme"), "{width}: {out}");
                    assert!(out.contains("• Juliana → Mateo"), "{width}: {out}");
                    assert!(out.contains("group words") && out.contains("direct words"));
                }
                // Too narrow for names and time on one row: the time goes
                // above them.
                let out = draw_sized(&mut view(&rows), 44, 24);
                let names = out
                    .lines()
                    .position(|l| l.contains("AVeryLongAgentName →"))
                    .expect(&out);
                let above = out.lines().nth(names - 1).unwrap();
                assert!(
                    above.starts_with('•') && above.contains("2026-10-09"),
                    "{out}"
                );
                assert!(!out.lines().nth(names).unwrap().contains("2026-"), "{out}");
            });
        }
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
        let mut view = ViewState::new(numbered(0..30), 1000, None);
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
        let mut view = ViewState::new(numbered(0..30), 1000, None);
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
        let mut view = ViewState::new(numbered(0..30), 1000, None);
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
        let mut view = ViewState::new(numbered(0..3), 1000, None);
        assert!(!view.handle_key(key(KeyCode::Char('/'))));
        assert!(!view.handle_key(key(KeyCode::Char('q'))));
        assert!(!view.handle_key(key(KeyCode::Esc)));
        assert_eq!(
            (view.prompt.as_deref(), view.query.as_deref()),
            (None, None)
        );
        view.handle_key(key(KeyCode::Char('/')));
        assert!(view.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(ViewState::new(Vec::new(), 1000, None).handle_key(key(KeyCode::Esc)));
    }

    #[test]
    fn snapshots_refresh_rows_and_the_oldest_are_dropped_past_the_cap() {
        let mut view = ViewState::new(numbered(0..3), 1000, None);
        let mut read = numbered(1..2);
        read[0].read_at = Some("2026-10-09T14:03:00.000000Z".into());
        // A later window that no longer holds the first row keeps it loaded.
        view.ingest(read);
        assert_eq!(view.rows.len(), 3);
        assert!(view.rows[1].message.read_at.is_some());

        draw(&mut view, 10);
        view.handle_key(key(KeyCode::Char('g')));
        view.ingest(numbered(3..MAX_ROWS as i64 + 5));
        assert_eq!(view.rows.len(), MAX_ROWS);
        assert_eq!(view.rows[0].message.id, 6);
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
        ViewState::new(rows, 1000, None)
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
        let mut view = ViewState::new(numbered(0..30), 1000, None);
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
        let mut view = ViewState::new(numbered(0..30), 1000, None);
        assert!(view.heights.iter().all(Option::is_none));
        draw(&mut view, 10);
        assert!(view.heights.iter().all(Option::is_some));
    }

    #[test]
    fn gap_detection_covers_an_empty_start_and_skips_one_message_windows() {
        let mut view = ViewState::new(Vec::new(), 3, None);
        view.ingest(numbered(10..13));
        assert_eq!(view.gaps, HashSet::from([11]));
        let mut view = ViewState::new(numbered(0..1), 1, None);
        for i in 1..5 {
            view.ingest(numbered(i..i + 1));
        }
        assert!(view.gaps.is_empty());
    }

    #[test]
    fn a_marker_on_the_oldest_kept_row_stays_drawn() {
        let mut view = ViewState::new(numbered(0..3), 3, None);
        view.ingest(numbered(10..13));
        // Three past the cap: rows 1-3 are dropped.
        view.ingest(numbered(13..MAX_ROWS as i64 + 10));
        // Row 11 (the first after the gap) is now the oldest kept row.
        assert_eq!(view.rows[0].message.id, 11);
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
        let mut view = ViewState::new(numbered(0..3), 1000, None);
        draw(&mut view, 10);
        view.ingest(numbered(0..3));
        assert!(view.heights.iter().all(Option::is_some));
    }

    #[test]
    fn a_full_snapshot_past_the_held_rows_marks_a_gap() {
        let mut view = ViewState::new(numbered(0..3), 3, None);
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
