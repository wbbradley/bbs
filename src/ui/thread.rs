//! A scrollable, following thread of messages with search: the core of
//! `bbs view` and of every thread in `bbs connect`. Key handling, any search
//! prompt and the title stay with each app.

use std::collections::HashSet;
use std::hash::Hash;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::proto::Message;
use crate::ui::message::{find_matches, fold, message_lines};

/// Loaded history kept while the view is open; the oldest rows are dropped
/// beyond this, never the stream.
pub(crate) const MAX_ROWS: usize = 10_000;

/// One row of a thread: a message, or anything else drawn as lines.
pub trait Row: PartialEq {
    /// Rows are kept in key order.
    type Key: Ord + Copy + Hash;
    /// A row's place may fall between rows already held, and it goes there;
    /// otherwise only rows newer than all those held are added.
    const MERGES: bool = false;
    fn key(&self) -> Self::Key;
    /// `gap`: messages may be missing before this row.
    fn lines(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>>;
    /// Whether the row matches a case-folded search.
    fn matches(&self, query: &str) -> bool;
}

impl Row for Message {
    type Key = i64;
    fn key(&self) -> i64 {
        self.id
    }
    fn lines(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        message_lines(self, gap, query, width)
    }
    /// The sender, recipient, group, or body as sent (markdown included).
    fn matches(&self, query: &str) -> bool {
        [
            Some(self.qualified_sender()),
            self.qualified_recipient(),
            self.to_group.as_ref().map(|g| format!("@{g}")),
        ]
        .iter()
        .flatten()
        .map(String::as_str)
        .chain([self.body.as_str()])
        .any(|text| !find_matches(text, query).is_empty())
    }
}

/// A scrollable, following thread of rows, messages by default: every row
/// loaded since it opened, and where the viewport sits in them. Fields are
/// crate-visible so the apps' tests can check its state.
pub struct ThreadView<R: Row = Message> {
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
                Err(i) if R::MERGES => {
                    self.matched.insert(i, self.matches(&m));
                    self.rows.insert(i, m);
                    self.heights.insert(i, None);
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
            let oldest = oldest.key();
            self.gaps.retain(|&id| id >= oldest);
        }
        self.layout(self.frame.width, self.frame.height);
    }

    /// Recompute line starts for a feed area, measuring only uncached rows.
    pub fn layout(&mut self, width: u16, height: usize) {
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
                    let gap = self.gaps.contains(&row.key());
                    let h = Paragraph::new(Text::from(row.lines(gap, None, width)))
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
                Paragraph::new("No messages yet. Waiting…".dark_gray()),
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
        let mut text = Text::default();
        for (i, row) in self.rows.iter().enumerate().skip(first) {
            let gap = self.gaps.contains(&row.key());
            text.lines
                .extend(row.lines(gap, self.query.as_deref(), area.width));
            let end = self.frame.starts.get(i + 1).unwrap_or(&self.frame.total);
            if *end >= top + self.frame.height {
                break;
            }
        }
        let offset = top - self.frame.starts[first];
        let feed = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(feed, area);
    }
}
