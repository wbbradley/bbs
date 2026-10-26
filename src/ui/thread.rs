//! A scrollable, following thread of messages with search: the core of
//! `bbs view` and of every thread in `bbs connect`. Key handling, any search
//! prompt and the title stay with each app.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Styled};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::ui::message::fold;

/// Columns kept clear at the right of every row, so text never runs into
/// the edge of the terminal.
pub(crate) const RIGHT_MARGIN: u16 = 1;

/// Loaded history kept while the view is open; the oldest rows are dropped
/// beyond this, never the stream.
pub(crate) const MAX_ROWS: usize = 10_000;

/// Quiet turn boundaries attached to visible content rather than extra rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnMarks {
    pub start: bool,
    pub end: bool,
}

/// One row of a thread: a message, or anything else drawn as lines.
pub trait Row: PartialEq {
    /// Rows are kept in key order.
    type Key: Ord + Copy + Hash;
    /// A row's place may fall between rows already held, and it goes there;
    /// otherwise only rows newer than all those held are added.
    const MERGES: bool = false;
    fn key(&self) -> Self::Key;
    /// `gap`: messages may be missing before this row.
    fn lines(&self, gap: bool, query: Option<&str>) -> Vec<Line<'_>>;
    /// Style applied across each rendered row, including wrapped continuations.
    fn style(&self) -> Style {
        Style::default()
    }
    /// Optional full-width surface for a blank first display row.
    fn separator_style(&self) -> Option<Style> {
        None
    }
    /// A left margin outside the wrapping area, including continuation lines.
    fn inset(&self) -> u16 {
        0
    }
    /// Content of that margin, in display-line order at the inner width.
    /// A line's background also fills the corresponding full-width row.
    fn gutter(&self, _gap: bool, _query: Option<&str>, _width: u16) -> Vec<Line<'_>> {
        Vec::new()
    }
    /// Whether the row matches a case-folded search.
    fn matches(&self, query: &str) -> bool;
    /// The row's lines at `width` columns, for a row that fits itself to
    /// the width rather than wrapping; others wrap their `lines`.
    fn lines_at(&self, gap: bool, query: Option<&str>, _width: u16) -> Vec<Line<'_>> {
        self.lines(gap, query)
    }
    /// Whether a blank line goes before this row, given the row before it
    /// (none at the top). Its height depends on that neighbour, so changing
    /// a row also re-measures the one after it.
    fn spaced_after(&self, _prev: Option<&Self>) -> bool {
        false
    }
    /// Whether a blank line goes after this row when it is the last one. The
    /// row before the newest is re-measured as rows arrive.
    fn spaced_last(&self) -> bool {
        false
    }
}

/// A scrollable, following thread of rows: every row
/// loaded since it opened, and where the viewport sits in them. Fields are
/// crate-visible so the apps' tests can check its state.
pub struct ThreadView<R: Row> {
    /// Oldest first, by key. Snapshots append newer rows and refresh rows
    /// they still contain, so history only grows while open.
    pub(crate) rows: Vec<R>,
    /// Wrapped line count of each row at a width, cached between frames.
    pub(crate) heights: Vec<Option<(u16, usize)>>,
    /// `None` follows the tail; otherwise the viewport's top line, anchored to
    /// a message so appends never move it. If the cap drops the anchored row,
    /// the view holds at the oldest kept row.
    pub(crate) paused: Option<Anchor<R::Key>>,
    /// Messages appended while paused.
    pub(crate) new_count: usize,
    /// The committed search, case-folded by `fold`.
    pub(crate) query: Option<String>,
    /// Whether each row matches `query`, kept in step with `rows`.
    pub(crate) matched: Vec<bool>,
    /// The server's window: each snapshot holds at most this many newest
    /// matching messages.
    pub(crate) window: usize,
    /// Rows that messages may be missing before: a full snapshot arrived whose
    /// oldest message was newer than every row held, so a burst outran it.
    pub(crate) gaps: HashSet<R::Key>,
    /// The first snapshot has arrived; later ones can reveal gaps.
    pub(crate) started: bool,
    /// Line layout that keys scroll against.
    pub(crate) frame: Geometry,
    /// Something drawn just below continues the last row (a pane of calls
    /// still running), so the last row keeps no closing blank of its own.
    continued: bool,
    turn_marks: HashMap<R::Key, TurnMarks>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Anchor<K> {
    pub(crate) id: K,
    pub(crate) line: usize,
}

/// Line layout of `rows` for the current terminal size; kept in step with
/// `rows`, so indexes into it are row indexes.
#[derive(Default)]
pub(crate) struct Geometry {
    pub(crate) width: u16,
    pub(crate) height: usize,
    /// First line of each row.
    pub(crate) starts: Vec<usize>,
    pub(crate) total: usize,
}

impl Geometry {
    pub(crate) fn bottom(&self) -> usize {
        self.total.saturating_sub(self.height)
    }
}

impl<R: Row> ThreadView<R> {
    pub fn new(rows: Vec<R>, window: usize) -> ThreadView<R> {
        let mut view = ThreadView {
            rows: Vec::new(),
            heights: Vec::new(),
            paused: None,
            new_count: 0,
            query: None,
            matched: Vec::new(),
            window,
            gaps: HashSet::new(),
            started: false,
            frame: Geometry::default(),
            continued: false,
            turn_marks: HashMap::new(),
        };
        view.ingest(rows);
        view
    }

    /// Merge a feed snapshot: refresh rows it still holds (e.g. `read_at`),
    /// append newer ones, and drop the oldest beyond MAX_ROWS.
    pub fn ingest(&mut self, snapshot: Vec<R>) {
        // A full window entirely newer than what is held may have skipped
        // messages; exactly --limit new ones look the same, so this can be a
        // false positive. A one-message window is always full, so it is not
        // checked.
        let newest = self.rows.last().map(Row::key);
        if let Some(first) = snapshot.first()
            && self.started
            && self.window > 1
            && snapshot.len() >= self.window
            && newest.is_none_or(|newest| first.key() > newest)
        {
            self.gaps.insert(first.key());
        }
        self.started = true;
        for m in snapshot {
            match self.rows.binary_search_by_key(&m.key(), Row::key) {
                Ok(i) if self.rows[i] != m => {
                    self.matched[i] = self.matches(&m);
                    self.rows[i] = m;
                    self.heights[i] = None;
                    self.remeasure(i + 1);
                }
                Ok(_) => {}
                Err(i) if i == self.rows.len() => {
                    self.matched.push(self.matches(&m));
                    self.rows.push(m);
                    self.heights.push(None);
                    // The old last row may have drawn a closing blank.
                    if let Some(before) = i.checked_sub(1) {
                        self.remeasure(before);
                    }
                    if self.paused.is_some() {
                        self.new_count += 1;
                    }
                }
                Err(i) if R::MERGES => {
                    self.matched.insert(i, self.matches(&m));
                    self.rows.insert(i, m);
                    self.heights.insert(i, None);
                    self.remeasure(i + 1);
                }
                // Older than everything kept: it was dropped, or never loaded.
                Err(_) => {}
            }
        }
        let excess = self.rows.len().saturating_sub(MAX_ROWS);
        self.rows.drain(..excess);
        self.heights.drain(..excess);
        self.matched.drain(..excess);
        if excess > 0 {
            // The new first row has no row before it now.
            self.remeasure(0);
        }
        if let Some(oldest) = self.rows.first() {
            let oldest = oldest.key();
            self.gaps.retain(|&id| id >= oldest);
        }
        self.layout(self.frame.width, self.frame.height);
    }

    /// Whether something drawn just below continues the last row; changing
    /// it re-measures that row.
    pub fn set_continued(&mut self, continued: bool) {
        if self.continued != continued {
            self.continued = continued;
            if let Some(last) = self.rows.len().checked_sub(1) {
                self.remeasure(last);
            }
        }
    }

    /// Forget row `i`'s measured height, if there is such a row.
    fn remeasure(&mut self, i: usize) {
        if let Some(height) = self.heights.get_mut(i) {
            *height = None;
        }
    }

    pub fn set_turn_marks(&mut self, marks: HashMap<R::Key, TurnMarks>) {
        for (i, row) in self.rows.iter().enumerate() {
            if self.turn_marks.get(&row.key()) != marks.get(&row.key()) {
                self.heights[i] = None;
            }
        }
        self.turn_marks = marks;
    }

    /// The columns row `row` wraps to in `width`: less its left inset, and
    /// less one column on the right so text never touches the edge, while
    /// at least one column is left.
    fn content_width(&self, row: &R, width: u16) -> u16 {
        let inner = width.saturating_sub(self.inset(row));
        if inner > RIGHT_MARGIN {
            inner - RIGHT_MARGIN
        } else {
            inner
        }
    }

    fn inset(&self, row: &R) -> u16 {
        if self.turn_marks.get(&row.key()).is_some_and(|m| m.start) {
            row.inset().max(2)
        } else {
            row.inset()
        }
    }

    /// Row `i`'s lines, with any blank line it asks for before it: shared by
    /// measuring and drawing so the two agree.
    fn row_lines(&self, i: usize, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        let row = &self.rows[i];
        let mut lines = row.lines_at(self.gaps.contains(&row.key()), query, width);
        if self.turn_marks.get(&row.key()).is_some_and(|m| m.end)
            && let Some(last) = lines
                .iter_mut()
                .rfind(|line| line.spans.iter().any(|s| !s.content.trim().is_empty()))
        {
            last.spans.push(Span::styled(
                " •",
                super::theme::style(super::theme::Role::Stop),
            ));
        }
        if row.spaced_after(i.checked_sub(1).map(|j| &self.rows[j])) {
            lines.insert(0, Line::default());
        }
        if i + 1 == self.rows.len() && !self.continued && row.spaced_last() {
            lines.push(Line::default());
        }
        lines
    }

    /// Recompute line starts for a feed area, measuring only uncached rows.
    pub fn layout(&mut self, width: u16, height: usize) {
        let mut starts = Vec::with_capacity(self.rows.len());
        let mut total = 0;
        for i in 0..self.rows.len() {
            starts.push(total);
            // Nothing is drawn before the first frame sizes the feed area.
            if width == 0 {
                continue;
            }
            total += match self.heights[i] {
                Some((w, h)) if w == width => h,
                _ => {
                    let inner_width = self.content_width(&self.rows[i], width);
                    let h = Paragraph::new(Text::from(self.row_lines(i, None, inner_width)))
                        .wrap(Wrap { trim: false })
                        .line_count(inner_width);
                    self.heights[i] = Some((width, h));
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
    pub fn top(&self) -> usize {
        let Some(anchor) = self.paused else {
            return self.frame.bottom();
        };
        let i = self.rows.partition_point(|r| r.key() < anchor.id);
        let start = self.frame.starts.get(i).copied().unwrap_or(0);
        let line = match self.rows.get(i) {
            // A resize can shorten the row the anchor line was measured in.
            Some(r) if r.key() == anchor.id => {
                let end = self.frame.starts.get(i + 1).unwrap_or(&self.frame.total);
                anchor.line.min(end.saturating_sub(start + 1))
            }
            _ => 0,
        };
        (start + line).min(self.frame.bottom())
    }

    /// Pause with `line` at the top of the viewport.
    pub fn scroll_to(&mut self, line: usize) {
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
            id: row.key(),
            line: line - self.frame.starts[i],
        });
    }

    /// Scroll down to `line`; following stays on when there is nothing below.
    pub fn scroll_down(&mut self, line: usize) {
        if self.paused.is_some() || line < self.frame.bottom() {
            self.scroll_to(line);
        }
    }

    pub fn follow(&mut self) {
        self.paused = None;
        self.new_count = 0;
    }

    fn matches(&self, row: &R) -> bool {
        self.query
            .as_deref()
            .is_some_and(|query| row.matches(query))
    }

    pub fn set_query(&mut self, query: &str) {
        let query: String = query.chars().map(fold).collect();
        self.query = (!query.is_empty()).then_some(query);
        self.matched = self.rows.iter().map(|r| self.matches(r)).collect();
    }

    /// Jump to the next (or previous) matching message, wrapping around. A
    /// match already in view at the tail counts as passed, so `n` moves on.
    pub fn jump(&mut self, forward: bool) {
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

    /// Search for `query` and go to the nearest match: the newest one when
    /// following, else the next one below the viewport.
    pub fn search(&mut self, query: &str) {
        self.set_query(query);
        if self.paused.is_some() {
            self.jump(true);
        } else if let Some(i) = self.matched.iter().rposition(|&m| m) {
            self.scroll_to(self.frame.starts[i]);
        }
    }

    /// Scroll up `lines`, pausing.
    pub fn scroll_up(&mut self, lines: usize) {
        let top = self.top();
        self.scroll_to(top.saturating_sub(lines));
    }

    /// Scroll down `lines`; following stays on at the tail.
    pub fn scroll_down_by(&mut self, lines: usize) {
        let top = self.top();
        self.scroll_down(top + lines);
    }

    /// Lines in view at the last layout.
    pub fn page(&self) -> usize {
        self.frame.height.max(1)
    }

    /// Whether the view follows new messages.
    pub fn following(&self) -> bool {
        self.paused.is_none()
    }

    /// The committed search and how many loaded messages match it.
    pub fn search_status(&self) -> Option<(&str, usize)> {
        let hits = self.matched.iter().filter(|&&m| m).count();
        self.query.as_deref().map(|q| (q, hits))
    }

    /// Draw the visible rows into `area`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.layout(area.width, area.height as usize);
        let top = self.top();
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No messages yet. Waiting…"
                        .set_style(super::theme::style(super::theme::Role::Muted)),
                ),
                area,
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
        for (i, row) in self.rows.iter().enumerate().skip(first) {
            let start = self.frame.starts[i];
            let end = *self.frame.starts.get(i + 1).unwrap_or(&self.frame.total);
            let visible_start = start.max(top);
            let visible_end = end.min(top + self.frame.height);
            let row_area = Rect::new(
                area.x,
                area.y + (visible_start - top) as u16,
                area.width,
                (visible_end - visible_start) as u16,
            );
            let inset = self.inset(row).min(row_area.width);
            let content_area = Rect::new(
                row_area.x + inset,
                row_area.y,
                self.content_width(row, row_area.width),
                row_area.height,
            );
            // The row's surface still reaches the right edge.
            frame.render_widget(Block::new().style(row.style()), row_area);
            let scroll = (visible_start - start).min(u16::MAX as usize) as u16;
            let mut gutter = row.gutter(
                self.gaps.contains(&row.key()),
                self.query.as_deref(),
                content_area.width,
            );
            if row.spaced_after(i.checked_sub(1).map(|j| &self.rows[j])) {
                gutter.insert(0, Line::default());
            }
            if self.turn_marks.get(&row.key()).is_some_and(|m| m.start) {
                let lines = self.row_lines(i, self.query.as_deref(), content_area.width);
                let first = lines
                    .iter()
                    .position(|line| line.spans.iter().any(|s| !s.content.trim().is_empty()));
                if let Some(first) = first {
                    let offset = Paragraph::new(Text::from(lines[..first].to_vec()))
                        .wrap(Wrap { trim: false })
                        .line_count(content_area.width);
                    gutter.resize(gutter.len().max(offset + 1), Line::default());
                    gutter[offset].spans = vec![Span::styled(
                        "• ",
                        super::theme::style(super::theme::Role::Go),
                    )];
                }
            }
            let paragraph =
                Paragraph::new(self.row_lines(i, self.query.as_deref(), content_area.width))
                    .style(row.style())
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0));
            frame.render_widget(paragraph, content_area);
            // Extend the gutter's background across the row without changing
            // participant colors or the text's search modifiers.
            for (y, line) in gutter
                .iter()
                .skip(scroll as usize)
                .take(row_area.height as usize)
                .enumerate()
            {
                if let Some(bg) = line.style.bg {
                    frame.render_widget(
                        Block::new().style(Style::default().bg(bg)),
                        Rect::new(row_area.x, row_area.y + y as u16, row_area.width, 1),
                    );
                }
            }
            if inset > 0 {
                frame.render_widget(
                    Paragraph::new(gutter)
                        .style(row.style())
                        .scroll((scroll, 0)),
                    Rect::new(row_area.x, row_area.y, inset, row_area.height),
                );
            }
            if visible_start == start
                && row_area.height > 0
                && let Some(style) = row.separator_style()
            {
                frame.render_widget(
                    Block::new().style(style),
                    Rect::new(row_area.x, row_area.y, row_area.width, 1),
                );
            }
            if end >= top + self.frame.height {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row spaced on both sides of each run of tool rows, as connect's
    /// timeline is.
    #[derive(Clone, Debug, PartialEq)]
    struct R {
        id: i64,
        tool: bool,
        text: &'static str,
    }

    impl Row for R {
        type Key = i64;
        const MERGES: bool = true;
        fn key(&self) -> i64 {
            self.id
        }
        fn lines(&self, _gap: bool, _query: Option<&str>) -> Vec<Line<'_>> {
            vec![Line::raw(self.text)]
        }
        fn matches(&self, _query: &str) -> bool {
            false
        }
        fn spaced_after(&self, prev: Option<&Self>) -> bool {
            prev.is_some_and(|p| p.tool) != self.tool
        }
        fn spaced_last(&self) -> bool {
            self.tool
        }
    }

    fn r(id: i64, tool: bool) -> R {
        R {
            id,
            tool,
            text: "row",
        }
    }

    #[test]
    fn turn_squares_follow_wrapping_and_scroll_without_extra_rows() {
        use crate::ui::theme::{self, Role};
        use ratatui::{Terminal, backend::TestBackend};
        for scheme in theme::testing::SCHEMES {
            theme::testing::with_scheme(scheme, || {
                // 13: the text and its end square fit one row short of the edge.
                for width in [8, 13] {
                    let mut view = ThreadView::new(
                        vec![R {
                            text: "abcdefgh",
                            ..r(1, true)
                        }],
                        100,
                    );
                    view.set_turn_marks(HashMap::from([(
                        1,
                        TurnMarks {
                            start: true,
                            end: true,
                        },
                    )]));
                    let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(
                                Block::new().style(theme::style(Role::Screen)),
                                f.area(),
                            );
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    theme::testing::assert_buffer(buffer);
                    assert_eq!(buffer[(0, 1)].symbol(), "•", "{scheme:?} {width}");
                    // The end square follows the text, wrapped at the
                    // width less the inset and the right margin.
                    let (end_x, end_y) = (1..8)
                        .flat_map(|y| (2..width).map(move |x| (x, y)))
                        .find(|&(x, y)| buffer[(x, y)].symbol() == "•")
                        .expect("the end square");
                    assert_eq!(end_y, if width == 8 { 2 } else { 1 }, "{width}");
                    assert!(end_x < width - RIGHT_MARGIN, "{width}");
                    let base = theme::style(Role::Screen);
                    assert_eq!(
                        Some(buffer[(0, 1)].fg),
                        theme::style(Role::Go).fg.or(base.fg)
                    );
                    assert_eq!(
                        Some(buffer[(end_x, end_y)].fg),
                        theme::style(Role::Stop).fg.or(base.fg)
                    );
                    assert_eq!(
                        view.frame.total,
                        end_y as usize + 2,
                        "only the run's existing blanks"
                    );
                    // Pausing on the wrapped continuation never repeats the go square.
                    if width == 8 {
                        view.layout(width, 2);
                        view.scroll_to(2);
                        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
                        terminal.draw(|f| view.render(f, f.area())).unwrap();
                        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), " ");
                        assert_eq!(terminal.backend().buffer()[(end_x, 0)].symbol(), "•");
                    }
                }
            });
        }
    }

    #[test]
    fn text_stops_one_column_short_of_the_right_edge_while_cards_reach_it() {
        use crate::proto::Message;
        use crate::ui::card::Card;
        use crate::ui::theme::{self, Role};
        use ratatui::{Terminal, backend::TestBackend};
        for scheme in theme::testing::SCHEMES {
            theme::testing::with_scheme(scheme, || {
                for width in [60u16, 80] {
                    // Bodies that once filled the card exactly, to its edge.
                    let fill = usize::from(width) - 2;
                    let bodies = [
                        "x".repeat(fill),
                        "界".repeat(fill / 2),
                        format!("| {} |\n| --- |\n| cell |", "h".repeat(fill - 4)),
                    ];
                    let rows: Vec<Card> = bodies
                        .iter()
                        .zip(1..)
                        .map(|(body, id)| {
                            Card::feed(
                                Message {
                                    id,
                                    created_at: "2026-10-16T18:00:00Z".into(),
                                    from_name: "Zed".into(),
                                    to_name: Some("Mateo".into()),
                                    to_owner: Some("ada".into()),
                                    body: body.clone(),
                                    ..Default::default()
                                },
                                None,
                            )
                        })
                        .collect();
                    let mut view = ThreadView::new(rows, 100);
                    let draw = |view: &mut ThreadView<Card>, width: u16| {
                        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                        terminal
                            .draw(|f| {
                                f.render_widget(
                                    Block::new().style(theme::style(Role::Screen)),
                                    f.area(),
                                );
                                view.render(f, f.area());
                            })
                            .unwrap();
                        terminal.backend().buffer().clone()
                    };
                    for query in [None, Some("x")] {
                        if let Some(query) = query {
                            view.set_query(query);
                        }
                        // Drawn wider first, so this is a resize.
                        draw(&mut view, width + 20);
                        let buffer = draw(&mut view, width);
                        theme::testing::assert_buffer(&buffer);
                        let text = |y: u16| {
                            (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                        };
                        let out: Vec<String> = (0..40).map(text).collect();
                        // Nothing starts in the last column. Ratatui lets a
                        // double-width character begun in the last text
                        // column spill one cell, which the margin now takes
                        // instead of the terminal's edge.
                        for y in 0..40 {
                            let spill = unicode_width::UnicodeWidthStr::width(
                                buffer[(width - 2, y)].symbol(),
                            ) == 2;
                            assert!(
                                buffer[(width - 1, y)].symbol() == " " || spill,
                                "{scheme:?} {width} row {y}: {out:#?}"
                            );
                        }
                        // Each body now wraps rather than touch the edge.
                        let xs = out.iter().filter(|l| l.starts_with("  x")).count();
                        assert_eq!(xs, 2, "{out:#?}");
                        assert!(out.iter().any(|l| l.contains('界')), "{out:#?}");
                        // The card's surface still fills the last column.
                        let card = theme::style(Role::Screen)
                            .patch(theme::style(Role::OtherConversation))
                            .bg;
                        let body = out.iter().position(|l| l.contains("xx")).unwrap() as u16;
                        assert_eq!(Some(buffer[(width - 1, body)].bg), card, "{scheme:?}");
                        let header = body - 1;
                        assert_eq!(
                            Some(buffer[(width - 1, header)].bg),
                            theme::style(Role::Screen)
                                .patch(theme::style(Role::OtherConversationHeader))
                                .bg
                        );
                    }
                    // Scrolling keeps the same geometry.
                    view.layout(width, 10);
                    view.scroll_to(3);
                    let buffer = {
                        let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
                        terminal.draw(|f| view.render(f, f.area())).unwrap();
                        terminal.backend().buffer().clone()
                    };
                    assert!((0..10).all(|y| buffer[(width - 1, y)].symbol() == " "));
                }
                // Too narrow for a margin: still drawn, never a panic.
                let mut view = ThreadView::new(vec![r(1, false)], 100);
                for width in [1u16, 2, 3] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
                    terminal.draw(|f| view.render(f, f.area())).unwrap();
                }
            });
        }
    }

    /// Lines laid out incrementally, and from scratch for the same rows.
    fn totals(view: &mut ThreadView<R>) -> (usize, usize) {
        view.layout(80, 50);
        let mut fresh = ThreadView::new(view.rows.clone(), view.window);
        fresh.layout(80, 50);
        (view.frame.total, fresh.frame.total)
    }

    #[test]
    fn neighbour_spacing_is_remeasured_on_insert_replace_and_eviction() {
        let mut view = ThreadView::new(vec![r(1, true), r(3, true)], 100);
        assert_eq!(totals(&mut view), (4, 4), "a run: a blank each side, tight");
        // A row landing inside the run splits it: blanks around each part.
        view.ingest(vec![r(2, false)]);
        assert_eq!(totals(&mut view), (7, 7));
        // Replacing that row with a tool row rejoins the run.
        view.ingest(vec![r(2, true)]);
        assert_eq!(totals(&mut view), (5, 5));
        // A row after the run moves its closing blank off the old last row.
        view.ingest(vec![r(4, false)]);
        assert_eq!(totals(&mut view), (6, 6));
        // Evicting the oldest row leaves a new first row with nothing before.
        let mut rows = vec![r(0, true), r(1, true)];
        rows.extend((2..MAX_ROWS as i64).map(|id| r(id, false)));
        let mut view = ThreadView::new(rows, 100);
        let before = totals(&mut view);
        assert_eq!(before.0, before.1);
        view.ingest(vec![r(MAX_ROWS as i64, false)]);
        let after = totals(&mut view);
        assert_eq!(after.0, after.1);
        // Row 0's blank went with it; row 1 now starts the run and takes one.
        assert_eq!(after.0, before.0);
    }
}
