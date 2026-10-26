//! The human's personal blocker queue (blockers-mine.v1), polled once a
//! second, and its full-terminal F2 modal. Answer drafts live only in this
//! session; each chain has at most one write in flight, retried with its own key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::composer::Composer;
use super::event::{Data, Request, SideError};
use super::theme::{self, Role};
use crate::proto;

/// After a blocker's text changes under the reader, Enter waits this long.
pub const GUARD: Duration = Duration::from_secs(1);
/// How long an update notice stays.
pub const NOTICE: Duration = Duration::from_secs(5);
/// Why the queue is unavailable: the server lacks the personal scope.
const UNSUPPORTED: &str = "Blockers need a newer server (blockers-mine.v1).";
/// Blockers listed above the selected one's text.
/// Refused blockers kept, with their drafts, after the server drops them.
const KEPT_REFUSALS: usize = 20;

#[derive(Debug, PartialEq)]
enum Support {
    Unknown,
    Supported,
    /// The server lacks blockers-mine.v1; polling stops until reload.
    Unsupported,
}

/// One write, kept whole so a retry repeats it exactly.
#[derive(Clone, Debug, PartialEq)]
enum Write {
    Answer { id: i64, answer: String },
    Cancel { id: i64 },
}

#[derive(Debug)]
struct Pending {
    key: String,
    write: Write,
    sent: Instant,
    /// No reply came: it may have landed. Enter retries, same key and payload.
    uncertain: bool,
}

/// A chain's local state, by its stable root.
struct Chain {
    answer: Composer,
    pending: Option<Pending>,
    error: Option<String>,
    /// The version and text last shown, to notice a replacement.
    seen: Option<(i64, String)>,
    guard_until: Option<Instant>,
}

impl Chain {
    fn new() -> Chain {
        Chain {
            answer: Composer::new("answer"),
            pending: None,
            error: None,
            seen: None,
            guard_until: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Focus {
    Answer,
    /// Open the creating agent's conversation.
    Discuss,
    Cancel,
    Confirm,
}

struct Panel {
    selected: Option<i64>,
    focus: Focus,
    /// Rows of the selected blocker's text scrolled past.
    scroll: u16,
    /// The first list row in view: it moves only to keep the selection in
    /// view, so the list stays put while the selection moves within it.
    list_start: usize,
}

/// What a key did in the open panel.
pub enum Outcome {
    Stay,
    /// The panel closed: back to the composer.
    Close,
    Request(Request),
    /// The panel closed to open the conversation of the agent that created
    /// the blocker: its stable id and name.
    Discuss {
        id: i64,
        name: String,
    },
    /// Not the panel's key (Ctrl-C): let the keymap have it.
    Pass,
}

pub struct Blockers {
    support: Support,
    polling: bool,
    loaded: bool,
    rows: Vec<proto::Blocker>,
    /// Rows the server no longer lists, kept for a write still out or a
    /// refusal to read; the server does not count them.
    held: Vec<i64>,
    active: u64,
    poll_error: Option<String>,
    /// The selected text's wrapped rows and the rows it had, at the last draw.
    text_rows: (u16, u16),
    chains: HashMap<i64, Chain>,
    panel: Option<Panel>,
    /// The selection when the panel last closed, and the rows' order then.
    remembered: Option<(i64, Vec<i64>)>,
    notice_until: Option<Instant>,
}

impl Blockers {
    pub fn new() -> Blockers {
        Blockers {
            support: Support::Unknown,
            polling: false,
            loaded: false,
            rows: Vec::new(),
            held: Vec::new(),
            active: 0,
            poll_error: None,
            text_rows: (0, 0),
            chains: HashMap::new(),
            panel: None,
            remembered: None,
            notice_until: None,
        }
    }

    /// The next poll for app `id`: one at a time, never once the
    /// server has refused the capability.
    pub fn poll(&mut self, id: u64) -> Option<Request> {
        if self.support == Support::Unsupported || self.polling {
            return None;
        }
        self.polling = true;
        Some(Request::Blockers { id })
    }

    /// A reload may ask a server that refused again.
    pub fn reload(&mut self) {
        if self.support == Support::Unsupported {
            self.support = Support::Unknown;
        }
    }

    /// Open blockers in scope, once known and supported.
    pub fn count(&self) -> Option<u64> {
        (self.support == Support::Supported && self.active > 0).then_some(self.active)
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// Open, with nothing left open or waiting: the page says so and stays.
    /// Blockers past the first page are still open: the next poll lists them.
    pub fn done(&self) -> bool {
        let busy = self
            .chains
            .values()
            .any(|c| c.pending.is_some() || c.error.is_some());
        self.panel.is_some()
            && self.loaded
            && self.support == Support::Supported
            && self.rows.is_empty()
            && self.active == 0
            && !busy
    }

    /// F2: open the panel, or say why not.
    pub fn open(&mut self) -> Result<(), String> {
        if self.support == Support::Unsupported {
            return Err("F5 retries · server needs blockers-mine.v1".into());
        }
        // Back where it was, or the nearest row still open.
        let roots: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        let selected = match &self.remembered {
            Some((root, old)) => nearest(Some(*root), old, &roots),
            None => roots.first().copied(),
        };
        self.panel = Some(Panel {
            list_start: 0,
            selected,
            focus: Focus::Answer,
            scroll: 0,
        });
        Ok(())
    }

    pub fn on_data(&mut self, id: u64, data: &Data, now: Instant) {
        match data {
            Data::Blockers { id: to, result } if *to == id => {
                self.polling = false;
                match result {
                    Ok(list) => self.listed(list, now),
                    Err(SideError::Unsupported(_)) => {
                        self.support = Support::Unsupported;
                        self.rows.clear();
                        self.active = 0;
                    }
                    // Background errors stay quiet; the panel shows the last.
                    Err(SideError::Refused(e) | SideError::Transient(e)) => {
                        self.poll_error = Some(e.clone());
                    }
                }
            }
            Data::BlockerWritten {
                id: to,
                key,
                result,
            } if *to == id => self.written(key, result),
            _ => {}
        }
    }

    fn listed(&mut self, list: &proto::ListBlockersResponse, now: Instant) {
        let shown = |rows: &[proto::Blocker]| -> Vec<(i64, String, bool, bool)> {
            rows.iter()
                .map(|b| (b.id, b.body.clone(), b.can_answer, b.can_cancel))
                .collect()
        };
        // Only what the server listed: rows kept here for a reply or a
        // refusal are not news on every poll.
        let listed: Vec<proto::Blocker> = self
            .rows
            .iter()
            .filter(|b| !self.held.contains(&b.root_id))
            .cloned()
            .collect();
        let changed = self.loaded
            && (shown(&listed) != shown(&list.blockers) || self.active != list.active_count);
        let old: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        self.support = Support::Supported;
        self.poll_error = None;
        // A row with a write still out stays until its reply, even when the
        // server already lists it resolved.
        let mut rows = list.blockers.clone();
        self.held.clear();
        for row in &self.rows {
            let keep = self
                .chains
                .get(&row.root_id)
                .is_some_and(|c| c.pending.is_some() || c.error.is_some());
            if keep && !rows.iter().any(|b| b.root_id == row.root_id) {
                rows.push(row.clone());
                self.held.push(row.root_id);
            }
        }
        // Only so many refusals are kept: the oldest go first.
        let refused: Vec<i64> = self
            .held
            .iter()
            .copied()
            .filter(|r| self.chains.get(r).is_some_and(|c| c.pending.is_none()))
            .collect();
        for root in refused
            .iter()
            .take(refused.len().saturating_sub(KEPT_REFUSALS))
        {
            rows.retain(|b| b.root_id != *root);
            self.held.retain(|r| r != root);
            self.chains.remove(root);
        }
        rows.sort_by_key(|b| b.root_id);
        self.rows = rows;
        self.active = list.active_count;
        let mut replaced = Vec::new();
        for row in &self.rows {
            let chain = self.chains.entry(row.root_id).or_insert_with(Chain::new);
            let current = (row.id, row.body.clone());
            if chain.seen.as_ref().is_some_and(|seen| *seen != current) {
                // Replaced under the reader: Enter waits a moment.
                chain.guard_until = Some(now + GUARD);
                replaced.push(row.root_id);
            }
            chain.seen = Some(current);
        }
        // Drafts go with their blocker, unless a write is still out.
        let roots: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        self.chains
            .retain(|root, chain| roots.contains(root) || chain.pending.is_some());
        if changed && self.panel.is_some() {
            self.notice_until = Some(now + NOTICE);
        }
        self.loaded = true;
        if let Some(panel) = &mut self.panel {
            let selected = nearest(panel.selected, &old, &roots);
            if selected != panel.selected || selected.is_some_and(|r| replaced.contains(&r)) {
                // New text to read from the top; a confirmation asked about
                // the old one, so it must be given again.
                panel.scroll = 0;
                if panel.focus == Focus::Confirm {
                    panel.focus = Focus::Cancel;
                }
            }
            panel.selected = selected;
            // Cancelling and discussing are offered only while possible.
            let gone = selected.is_some_and(|r| self.held.contains(&r));
            let row = self.rows.iter().find(|b| Some(b.root_id) == selected);
            let lost = match panel.focus {
                Focus::Cancel | Focus::Confirm => !gone && !row.is_some_and(|b| b.can_cancel),
                Focus::Discuss => row.and_then(agent_creator).is_none(),
                Focus::Answer => false,
            };
            if lost {
                panel.focus = Focus::Answer;
            }
        }
    }

    fn written(
        &mut self,
        key: &str,
        result: &Result<Box<proto::BlockerMutationResponse>, SideError>,
    ) {
        let Some(root) = self
            .chains
            .iter()
            .find(|(_, c)| c.pending.as_ref().is_some_and(|p| p.key == key))
            .map(|(root, _)| *root)
        else {
            return;
        };
        let chain = self.chains.get_mut(&root).expect("found by its key");
        match result {
            Ok(_) => {
                let pending = chain.pending.take().expect("found by its key");
                let version = match pending.write {
                    Write::Answer { id, .. } | Write::Cancel { id } => id,
                };
                if self
                    .rows
                    .iter()
                    .any(|b| b.root_id == root && b.id != version)
                {
                    // A newer version of the chain came first: it stays for
                    // the next poll to settle. The delivered answer leaves
                    // the draft, so Enter cannot send it again to the new
                    // version; a cancel sent no text, and the draft stays.
                    if let Write::Answer { answer, .. } = &pending.write {
                        chain.answer.clear_if(answer);
                    }
                    chain.error = None;
                    return;
                }
                // Resolved: gone from the queue before the next poll says so.
                self.chains.remove(&root);
                let old: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
                if old.contains(&root) {
                    self.rows.retain(|b| b.root_id != root);
                    if !self.held.contains(&root) {
                        self.active = self.active.saturating_sub(1);
                    }
                }
                self.held.retain(|r| *r != root);
                let roots: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
                if let Some(panel) = &mut self.panel {
                    if panel.selected == Some(root) {
                        panel.focus = Focus::Answer;
                    }
                    panel.selected = nearest(panel.selected, &old, &roots);
                }
            }
            Err(SideError::Transient(_)) => {
                if let Some(pending) = &mut chain.pending {
                    pending.uncertain = true;
                }
            }
            Err(SideError::Refused(e) | SideError::Unsupported(e)) => {
                // A definite refusal (a conflict, a retired creator): the
                // draft stays for another try or another blocker.
                chain.pending = None;
                chain.error = Some(e.clone());
                if let Some(panel) = &mut self.panel
                    && panel.focus == Focus::Confirm
                {
                    panel.focus = Focus::Cancel;
                }
            }
        }
    }

    /// Close the panel, remembering where it was.
    pub(super) fn close(&mut self) {
        if let Some(panel) = self.panel.take() {
            let roots = self.rows.iter().map(|b| b.root_id).collect();
            self.remembered = panel.selected.map(|root| (root, roots));
        }
    }

    /// Drop a kept refusal and its draft.
    fn dismiss(&mut self, root: i64) {
        let old: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        self.rows.retain(|b| b.root_id != root);
        self.held.retain(|r| *r != root);
        self.chains.remove(&root);
        let roots: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        if let Some(panel) = &mut self.panel {
            panel.selected = nearest(panel.selected, &old, &roots);
            panel.focus = Focus::Answer;
            panel.scroll = 0;
        }
    }

    /// When the panel next changes by itself.
    pub fn next_frame(&self, now: Instant) -> Option<Instant> {
        self.panel.as_ref()?;
        let guard = self.selected_chain().and_then(|c| c.guard_until);
        let spinning = self
            .chains
            .values()
            .any(|c| c.pending.as_ref().is_some_and(|p| !p.uncertain));
        [
            guard,
            self.notice_until,
            spinning.then(|| now + Duration::from_millis(100)),
        ]
        .into_iter()
        .flatten()
        .filter(|at| *at > now)
        .min()
    }

    fn selected(&self) -> Option<&proto::Blocker> {
        let root = self.panel.as_ref()?.selected?;
        self.rows.iter().find(|b| b.root_id == root)
    }

    fn selected_chain(&self) -> Option<&Chain> {
        self.chains.get(&self.selected()?.root_id)
    }

    pub fn paste(&mut self, text: &str) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if self.panel.as_ref().map(|p| p.focus) != Some(Focus::Answer) || !row.can_answer {
            return;
        }
        let chain = self.chains.entry(row.root_id).or_insert_with(Chain::new);
        if chain.pending.is_none() {
            chain.answer.paste(text);
        }
    }

    pub fn key(&mut self, id: u64, key: KeyEvent, now: Instant) -> Outcome {
        let key = if key.code == KeyCode::F(2) && key.modifiers.is_empty() {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            key
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Outcome::Pass;
        }
        let Some(panel) = &mut self.panel else {
            return Outcome::Pass;
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if panel.focus == Focus::Confirm => {
                panel.focus = Focus::Answer;
                return Outcome::Stay;
            }
            KeyCode::Esc => {
                self.close();
                return Outcome::Close;
            }
            KeyCode::Up | KeyCode::Down if alt => {
                let roots: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
                let at = panel
                    .selected
                    .and_then(|r| roots.iter().position(|x| *x == r))
                    .unwrap_or(0);
                let to = if key.code == KeyCode::Up {
                    at.saturating_sub(1)
                } else {
                    (at + 1).min(roots.len().saturating_sub(1))
                };
                panel.selected = roots.get(to).copied();
                panel.focus = Focus::Answer;
                panel.scroll = 0;
                return Outcome::Stay;
            }
            // Page through the selected blocker's text.
            KeyCode::PageUp | KeyCode::PageDown => {
                let (rows, shown) = self.text_rows;
                let page = shown.saturating_sub(1).max(1);
                panel.scroll = if key.code == KeyCode::PageUp {
                    panel.scroll.saturating_sub(page)
                } else {
                    (panel.scroll + page).min(rows.saturating_sub(shown))
                };
                return Outcome::Stay;
            }
            _ => {}
        }
        let Some(row) = self.selected().cloned() else {
            return Outcome::Stay;
        };
        // Ahead of retries and the replacement guard: discussing never
        // answers or cancels.
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.panel.as_ref().map(|p| p.focus) == Some(Focus::Discuss)
        {
            if let (Some(name), Some(creator)) = (agent_creator(&row), &row.creator) {
                return Outcome::Discuss {
                    id: creator.id,
                    name,
                };
            }
            self.panel.as_mut().expect("open").focus = Focus::Answer;
            return Outcome::Stay;
        }
        let gone = self.held.contains(&row.root_id);
        if gone
            && key.code == KeyCode::Enter
            && self.panel.as_ref().map(|p| p.focus) == Some(Focus::Cancel)
            && self
                .chains
                .get(&row.root_id)
                .is_some_and(|c| c.pending.is_none())
        {
            // Dismiss a refusal kept for reading.
            self.dismiss(row.root_id);
            return Outcome::Stay;
        }
        let panel = self.panel.as_mut().expect("open");
        let chain = self.chains.entry(row.root_id).or_insert_with(Chain::new);
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                // Answer, then discuss, then cancel, skipping what is not offered.
                let mut chain = vec![Focus::Answer];
                if agent_creator(&row).is_some() {
                    chain.push(Focus::Discuss);
                }
                if row.can_cancel || gone {
                    chain.push(Focus::Cancel);
                }
                let current = match panel.focus {
                    Focus::Confirm => Focus::Cancel,
                    focus => focus,
                };
                let at = chain.iter().position(|f| *f == current).unwrap_or(0);
                let step = if key.code == KeyCode::Tab {
                    1
                } else {
                    chain.len() - 1
                };
                panel.focus = chain[(at + step) % chain.len()];
                Outcome::Stay
            }
            // Just replaced: no Enter answers, cancels or confirms yet.
            KeyCode::Enter if chain.guard_until.is_some_and(|until| now < until) => Outcome::Stay,
            KeyCode::Enter if key.modifiers.is_empty() => {
                if let Some(pending) = &mut chain.pending {
                    if !pending.uncertain {
                        return Outcome::Stay;
                    }
                    // The same key and payload: it lands at most once.
                    pending.uncertain = false;
                    pending.sent = now;
                    return Outcome::Request(request(id, &pending.key, &pending.write));
                }
                match panel.focus {
                    Focus::Discuss => Outcome::Stay,
                    // Permission is checked at each step, not only on Tab.
                    Focus::Cancel | Focus::Confirm if !row.can_cancel || gone => {
                        panel.focus = Focus::Answer;
                        Outcome::Stay
                    }
                    Focus::Cancel => {
                        panel.focus = Focus::Confirm;
                        Outcome::Stay
                    }
                    Focus::Confirm => {
                        panel.focus = Focus::Cancel;
                        submit(id, chain, Write::Cancel { id: row.id }, now)
                    }
                    Focus::Answer => {
                        // A blocker no longer open takes no answer.
                        if !row.can_answer || gone {
                            return Outcome::Stay;
                        }
                        let answer = chain.answer.draft().text;
                        if answer.trim().is_empty() {
                            return Outcome::Stay;
                        }
                        submit(id, chain, Write::Answer { id: row.id, answer }, now)
                    }
                }
            }
            _ if panel.focus == Focus::Answer && row.can_answer && chain.pending.is_none() => {
                chain.answer.key(key, 0);
                Outcome::Stay
            }
            _ => Outcome::Stay,
        }
    }

    fn answer_height(&self, width: u16) -> u16 {
        match (self.selected(), self.selected_chain()) {
            // A draft written before the row became read only still shows.
            (Some(row), Some(chain))
                if row.can_answer
                    || chain.error.is_some()
                    || !chain.answer.draft().text.is_empty() =>
            {
                chain.answer.height(width, 6)
            }
            _ => 0,
        }
    }

    /// The header, list, and the selected blocker's text and recipient.
    #[cfg(test)]
    fn lines_above(&self, width: u16) -> Vec<Line<'static>> {
        let Parts {
            mut head,
            list,
            more,
            selected,
            body,
            label,
        } = self.parts(width);
        head.extend(list.into_iter().map(|(_, line)| line));
        head.extend(more);
        head.extend(selected);
        head.extend(body);
        head.extend(label);
        head
    }

    /// The list's title bar, the same whatever is selected, with the keys at
    /// the right when they fit.
    fn title(&self, width: usize) -> Line<'static> {
        let left = " Open blockers";
        // The keys at the right, shortened to the way out when narrow.
        let keys = [" Alt-↑/↓ · Tab · Esc ", " Esc "]
            .into_iter()
            .find(|keys| left.width() + keys.width() < width);
        let text = match keys {
            Some(keys) => {
                let gap = width - left.width() - keys.width();
                format!("{left}{}{keys}", " ".repeat(gap))
            }
            _ => left.to_string(),
        };
        theme::clip_line(Line::raw(text), width).style(theme::style(Role::InfoTitle))
    }

    /// The header and list; the selected blocker restated, its text, which
    /// gets what room is left, and the line naming who an answer goes to.
    fn parts(&self, width: u16) -> Parts {
        let width = usize::from(width);
        let Some(panel) = &self.panel else {
            return Parts::default();
        };
        let mut lines = vec![self.title(width)];
        if self.support == Support::Unsupported {
            lines.push(Line::default());
            lines.push(Line::styled(
                format!("  {UNSUPPORTED} Esc returns."),
                theme::style(Role::Warning),
            ));
            return Parts {
                head: lines,
                ..Parts::default()
            };
        }
        if self.done() {
            lines.push(Line::default());
            lines.push(Line::styled(
                "  No open blockers.",
                theme::style(Role::Success),
            ));
            return Parts {
                head: lines,
                ..Parts::default()
            };
        }
        if !self.loaded {
            lines.push(Line::styled("Loading…", theme::dim()));
            return Parts {
                head: lines,
                ..Parts::default()
            };
        }
        let mut list = Vec::new();
        for row in &self.rows {
            let chosen = panel.selected == Some(row.root_id);
            let mut line = Line::from(vec![
                Span::raw(if chosen { "▸ " } else { "  " }),
                Span::raw(format!("#{} ", row.id)),
                Span::styled(format!("@{}", row.group), theme::style(Role::Group)),
                Span::raw(" "),
                Span::styled(creator(row), theme::style(Role::Agent)),
                Span::raw("  "),
                Span::raw(row.body.lines().next().unwrap_or_default().to_string()),
            ]);
            if chosen {
                line = line.style(theme::style(Role::Selection));
            }
            list.push((row.root_id, theme::clip_line(line, width)));
        }
        let mut more = Vec::new();
        let unlisted = self.active.saturating_sub(self.rows.len() as u64);
        if unlisted > 0 {
            more.push(Line::styled(
                format!("  + {unlisted} more not shown"),
                theme::dim(),
            ));
        }
        if self.rows.is_empty() && self.active > 0 {
            more.push(Line::styled("Loading more…", theme::dim()));
        }
        let Some(row) = self.selected() else {
            return Parts {
                head: lines,
                list,
                more,
                ..Parts::default()
            };
        };
        let selected = Line::from(vec![
            Span::raw(format!("  #{} ", row.id)),
            Span::styled(format!("@{}", row.group), theme::style(Role::Group)),
            Span::raw(" "),
            Span::styled(creator(row), theme::style(Role::Agent)),
        ])
        .style(Style::new().add_modifier(Modifier::BOLD));
        // Markdown, laid out for the card less its two-column margin.
        let mut body = Vec::new();
        let markdown =
            crate::ui::markdown::render_at(&row.body, Some(width.saturating_sub(2) as u16));
        for line in markdown.lines {
            let spans = line
                .spans
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), s.style))
                .collect::<Vec<_>>();
            body.push(Line::from(spans).style(line.style));
        }
        let label = if self.held.contains(&row.root_id) {
            Line::styled("No longer open: your draft is kept below.", theme::dim())
        } else if row.can_answer {
            Line::from(vec![
                Span::styled("answer → ", theme::dim()),
                Span::styled(creator(row), theme::style(Role::Agent)),
                Span::raw(" "),
                Span::styled(format!("@{}", row.group), theme::style(Role::Group)),
            ])
        } else if row.can_cancel {
            // The server allows answering whoever may cancel, unless the
            // creating agent is gone.
            Line::styled(
                format!(
                    "Can't answer: {} is no longer available; it can still be cancelled.",
                    creator(row)
                ),
                theme::dim(),
            )
        } else {
            Line::styled(
                format!(
                    "Read only: answering needs an agent of yours in @{}.",
                    row.group
                ),
                theme::dim(),
            )
        };
        Parts {
            head: lines,
            list,
            more,
            selected: Some(theme::clip_line(selected, width)),
            body,
            label: Some(theme::clip_line(label, width)),
        }
    }

    /// The Cancel action, or its confirmation, and the state line.
    fn lines_below(&self, width: u16, now: Instant) -> Vec<Line<'static>> {
        let (Some(panel), Some(row), Some(chain)) =
            (&self.panel, self.selected(), self.selected_chain())
        else {
            return vec![Line::default(), self.status_line(None, now)];
        };
        let gone = self.held.contains(&row.root_id) && chain.pending.is_none();
        let button = |text: String, focus: Focus| {
            let style = if panel.focus == focus {
                theme::style(Role::Selection)
            } else {
                theme::dim()
            };
            Span::styled(text, style)
        };
        let action = if panel.focus == Focus::Confirm && !gone {
            Line::from(vec![
                Span::styled("Cancel this blocker? ", theme::style(Role::Warning)),
                Span::styled("Enter confirms · Esc returns", theme::dim()),
            ])
        } else {
            let cancel = if gone {
                Some("[Dismiss]")
            } else {
                row.can_cancel.then_some("[Cancel]")
            };
            let mut spans = Vec::new();
            if let Some(agent) = agent_creator(row) {
                // A long name shortens; the buttons after it and Tab stay.
                let rest = cancel.map_or(0, |c| c.width() + 1) + " Tab".width();
                let room = usize::from(width)
                    .saturating_sub(rest + "[discuss with ]".width())
                    .max(1);
                let name = theme::clip_line(Line::raw(agent), room).to_string();
                spans.push(button(format!("[discuss with {name}]"), Focus::Discuss));
            }
            if let Some(cancel) = cancel {
                if !spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                spans.push(button(cancel.into(), Focus::Cancel));
            }
            if !spans.is_empty() {
                spans.push(Span::styled(" Tab", theme::dim()));
            }
            Line::from(spans)
        };
        vec![action, self.status_line(Some(chain), now)]
    }

    fn status_line(&self, chain: Option<&Chain>, now: Instant) -> Line<'static> {
        if let Some(chain) = chain {
            if let Some(pending) = &chain.pending {
                return if pending.uncertain {
                    Line::styled("Not confirmed: Enter retries", theme::style(Role::Warning))
                } else {
                    let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                    let tick =
                        (now.saturating_duration_since(pending.sent).as_millis() / 100) as usize;
                    Line::styled(
                        format!("{} Sending…", frames[tick % frames.len()]),
                        theme::dim().add_modifier(Modifier::ITALIC),
                    )
                };
            }
            if let Some(error) = &chain.error {
                return Line::styled(error.clone(), theme::error());
            }
            if chain.guard_until.is_some_and(|until| now < until) {
                return Line::styled(
                    "This blocker just changed: read it before answering.",
                    theme::style(Role::Warning),
                );
            }
        }
        if self.notice_until.is_some_and(|until| now < until) {
            return Line::styled("Blockers updated.", theme::style(Role::Success));
        }
        if let Some(error) = &self.poll_error {
            return Line::styled(format!("Not refreshed: {error}"), theme::dim());
        }
        Line::default()
    }

    /// The list in the top half of `area`, the selected blocker and its
    /// answer in the bottom half, each against its half's foot: neither half
    /// moves for what the other holds.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, now: Instant) {
        let Parts {
            head,
            list,
            more,
            selected,
            body,
            label,
        } = self.parts(area.width);
        let [top, area] =
            Layout::vertical([Constraint::Length(area.height / 2), Constraint::Fill(1)])
                .areas(area);
        frame.render_widget(ratatui::widgets::Block::new().style(theme::base()), top);
        frame.render_widget(ratatui::widgets::Block::new().style(theme::base()), area);
        // The top half: as many list rows as fit, the window moving only to
        // keep the selection in view.
        let room = usize::from(top.height).saturating_sub(head.len() + more.len());
        let at = self
            .panel
            .as_ref()
            .and_then(|p| p.selected)
            .and_then(|r| list.iter().position(|(root, _)| *root == r))
            .unwrap_or(0);
        let start = match &mut self.panel {
            Some(panel) => {
                let mut start = panel.list_start.min(at);
                if room > 0 && at >= start + room {
                    start = at + 1 - room;
                }
                start = start.min(list.len().saturating_sub(room));
                panel.list_start = start;
                start
            }
            None => 0,
        };
        let shown_list = list.len().saturating_sub(start).min(room);
        let head_rows = (head.len() + shown_list + more.len()).min(usize::from(top.height)) as u16;
        let head_area = Rect {
            y: top.bottom() - head_rows,
            height: head_rows,
            ..top
        };
        frame.render_widget(
            ratatui::widgets::Block::new().style(theme::style(Role::InfoSurface)),
            head_area,
        );
        let mut head: Vec<Line> = head;
        head.extend(
            list.into_iter()
                .skip(start)
                .take(shown_list)
                .map(|(_, line)| line),
        );
        head.extend(more);
        // The bottom half: the selected blocker, its text, who an answer goes
        // to, the answer box and the actions, all against the foot.
        let below = self.lines_below(area.width, now);
        let inset = area.width.min(2);
        let body = Paragraph::new(body).wrap(Wrap { trim: false });
        let rows = body.line_count(area.width - inset) as u16;
        let label_gap = u16::from(label.is_some());
        // A blank row parts the list from the blocker restated below it.
        let list_gap = u16::from(selected.is_some());
        let fixed = [
            label_gap,
            list_gap,
            selected.iter().count() as u16,
            label.iter().count() as u16,
            below.len() as u16,
        ]
        .iter()
        .fold(0u16, |sum, rows| sum.saturating_add(*rows));
        // A tall draft takes no more than the half has.
        let answer = self
            .answer_height(area.width)
            .min(area.height.saturating_sub(fixed));
        let room = area.height.saturating_sub(fixed + answer);
        let shown = rows.min(room);
        let [
            _,
            _,
            selected_area,
            body_area,
            _,
            label_area,
            middle,
            bottom,
        ] = Layout::vertical([
            Constraint::Length(room - shown),
            Constraint::Length(list_gap),
            Constraint::Length(selected.iter().count() as u16),
            Constraint::Length(shown),
            Constraint::Length(label_gap),
            Constraint::Length(label.iter().count() as u16),
            Constraint::Length(answer),
            Constraint::Length(below.len() as u16),
        ])
        .areas(area);
        let mut head = head.into_iter();
        if let Some(title) = head.next() {
            let bar = Rect {
                height: head_area.height.min(1),
                ..head_area
            };
            let style = title.style;
            frame.render_widget(Paragraph::new(title).style(style), bar);
        }
        let rest = Rect {
            y: head_area.y + head_area.height.min(1),
            height: head_area.height.saturating_sub(1),
            ..head_area
        };
        frame.render_widget(Paragraph::new(head.collect::<Vec<_>>()), rest);
        frame.render_widget(Paragraph::new(Vec::from_iter(selected)), selected_area);
        // Keep the margin outside wrapping, just like conversation messages.
        let body_area = Rect::new(
            body_area.x + inset,
            body_area.y,
            body_area.width - inset,
            body_area.height,
        );
        self.text_rows = (rows, body_area.height);
        let mut label = label;
        if let Some(panel) = &mut self.panel {
            panel.scroll = panel.scroll.min(rows.saturating_sub(body_area.height));
            if rows > body_area.height
                && let Some(line) = label.take()
            {
                // The rest is a page key away; the hint keeps its room.
                let last = (panel.scroll + body_area.height).min(rows);
                let hint = format!("  PgUp/PgDn text {last}/{rows}");
                let room = usize::from(area.width).saturating_sub(hint.width());
                let mut line = theme::clip_line(line, room);
                line.spans.push(Span::styled(hint, theme::dim()));
                label = Some(line);
            }
            frame.render_widget(body.scroll((panel.scroll, 0)), body_area);
        }
        frame.render_widget(Paragraph::new(Vec::from_iter(label)), label_area);
        frame.render_widget(Paragraph::new(below), bottom);
        let Some((root, editable)) = self
            .selected()
            .map(|b| (b.root_id, b.can_answer && !self.held.contains(&b.root_id)))
        else {
            return;
        };
        let focused = editable && self.panel.as_ref().map(|p| p.focus) == Some(Focus::Answer);
        if let Some(chain) = self.chains.get_mut(&root) {
            let pending = chain.pending.is_some();
            chain.answer.render(frame, middle, focused && !pending);
            if pending {
                // Darkened and italic until the server answers.
                let buffer = frame.buffer_mut();
                for y in middle.top()..middle.bottom() {
                    for x in middle.left()..middle.right() {
                        let cell = &mut buffer[(x, y)];
                        cell.modifier.insert(Modifier::ITALIC | Modifier::DIM);
                    }
                }
            }
        }
    }
}

/// The page's text: the top half's title, notes and list, and the bottom
/// half's selected blocker above the answer box.
#[derive(Default)]
struct Parts {
    /// The title bar, and what the list says instead of rows, if anything.
    head: Vec<Line<'static>>,
    /// Every listed row, by root, for the view to window.
    list: Vec<(i64, Line<'static>)>,
    /// Under the list: rows not listed, or still loading.
    more: Vec<Line<'static>>,
    /// The selected blocker, restated above its text.
    selected: Option<Line<'static>>,
    body: Vec<Line<'static>>,
    /// Who an answer goes to, or why there is none.
    label: Option<Line<'static>>,
}

/// Where the selection goes when its row leaves: the row now at its place.
fn nearest(selected: Option<i64>, old: &[i64], new: &[i64]) -> Option<i64> {
    if let Some(root) = selected
        && new.contains(&root)
    {
        return Some(root);
    }
    let at = selected
        .and_then(|r| old.iter().position(|x| *x == r))
        .unwrap_or(0);
    // The first survivor at or after the old place, else the last before it.
    old.iter()
        .skip(at)
        .find(|r| new.contains(r))
        .or_else(|| old.iter().take(at).rev().find(|r| new.contains(r)))
        .copied()
        .or_else(|| new.first().copied())
}

/// The creator's name when it is an agent, whose conversation can open.
fn agent_creator(row: &proto::Blocker) -> Option<String> {
    row.creator
        .as_ref()
        .filter(|p| p.kind() == proto::Kind::Agent && !p.name.is_empty())
        .map(|p| p.name.clone())
}

fn creator(row: &proto::Blocker) -> String {
    row.creator
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "?".into())
}

fn submit(id: u64, chain: &mut Chain, write: Write, now: Instant) -> Outcome {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let key = format!(
        "connect-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    chain.error = None;
    let request = request(id, &key, &write);
    chain.pending = Some(Pending {
        key,
        write,
        sent: now,
        uncertain: false,
    });
    Outcome::Request(request)
}

fn request(id: u64, key: &str, write: &Write) -> Request {
    match write {
        Write::Answer {
            id: blocker,
            answer,
        } => Request::AnswerBlocker {
            id,
            req: proto::AnswerBlockerRequest {
                id: *blocker,
                answer: answer.clone(),
                request_key: key.to_string(),
            },
        },
        Write::Cancel { id: blocker } => Request::CancelBlocker {
            id,
            req: proto::CancelBlockerRequest {
                id: *blocker,
                request_key: key.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: u64 = 7;

    fn row(id: i64, root: i64, creator: &str, body: &str) -> proto::Blocker {
        proto::Blocker {
            id,
            root_id: root,
            group: "crew".into(),
            creator: Some(proto::Principal {
                kind: proto::Kind::Agent.into(),
                id: 100 + root,
                name: creator.into(),
                owner: Some("ada".into()),
            }),
            body: body.into(),
            status: proto::BlockerStatus::BlockerOpen.into(),
            can_answer: true,
            can_cancel: true,
            ..Default::default()
        }
    }

    fn list(rows: Vec<proto::Blocker>) -> Data {
        Data::Blockers {
            id: SCREEN,
            result: Ok(Box::new(proto::ListBlockersResponse {
                active_count: rows.len() as u64,
                blockers: rows,
                ..Default::default()
            })),
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn typed(blockers: &mut Blockers, text: &str, now: Instant) {
        for c in text.chars() {
            assert!(matches!(
                blockers.key(SCREEN, press(KeyCode::Char(c)), now),
                Outcome::Stay
            ));
        }
    }

    /// Two open blockers, polled once, with the panel open.
    fn open(now: Instant) -> Blockers {
        let mut blockers = Blockers::new();
        assert!(blockers.poll(SCREEN).is_some());
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(1, 1, "Zed", "first?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            now,
        );
        blockers.open().unwrap();
        blockers
    }

    fn answer_request(outcome: Outcome) -> proto::AnswerBlockerRequest {
        match outcome {
            Outcome::Request(Request::AnswerBlocker { id, req }) => {
                assert_eq!(id, SCREEN);
                req
            }
            _ => panic!("expected an answer"),
        }
    }

    fn written(key: &str, result: Result<Box<proto::BlockerMutationResponse>, SideError>) -> Data {
        Data::BlockerWritten {
            id: SCREEN,
            key: key.into(),
            result,
        }
    }

    /// The card's rows at `width` x `height`, and its buffer.
    fn drawn(
        blockers: &mut Blockers,
        width: u16,
        height: u16,
        now: Instant,
    ) -> (Vec<String>, ratatui::buffer::Buffer) {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| blockers.render(f, f.area(), now))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        crate::ui::theme::testing::assert_buffer(&buffer);
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (rows, buffer)
    }

    /// The row restating the selection: below the list, not a list row.
    fn restated(rows: &[String], what: &str) -> usize {
        rows.iter()
            .rposition(|r| r.starts_with(&format!("  {what}")))
            .unwrap_or_else(|| panic!("{what}: {rows:#?}"))
    }

    #[test]
    fn short_details_sit_just_above_the_answer_with_spare_rows_above() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for (width, height) in [(60u16, 24u16), (80, 24), (80, 40)] {
                    let now = Instant::now();
                    let mut blockers = open(now);
                    let (rows, buffer) = drawn(&mut blockers, width, height, now);
                    let out = rows.join("\n");
                    // The selection restated, its text, who it answers.
                    // The list at the foot of the top half.
                    let top = usize::from(height / 2);
                    let title = top - 3;
                    assert!(rows[title].starts_with(" Open blockers"), "{out}");
                    assert!(rows[top - 2].starts_with("▸ #1 @crew Zed  first?"), "{out}");
                    assert!(
                        rows[top - 1].starts_with("  #2 @crew Mateo  second?"),
                        "{out}"
                    );
                    let at = restated(&rows, "#1 @crew Zed");
                    assert!(at > top, "{out}");
                    assert_eq!(rows[at - 1].trim(), "", "a blank row before it: {out}");
                    assert_eq!(rows[at].trim_end(), "  #1 @crew Zed", "{out}");
                    assert!(rows[at + 1].starts_with("  first?"), "{out}");
                    assert_eq!(
                        rows[at + 2].trim(),
                        "",
                        "one gap before the recipient: {out}"
                    );
                    assert!(rows[at + 3].starts_with("answer → Zed @crew"), "{out}");
                    // One blank row, then the answer box.
                    assert_eq!(rows[at + 4].trim(), "", "{out}");
                    if let Some(bg) = theme::style(Role::HumanSurface).bg {
                        assert_eq!(buffer[(0, at as u16 + 5)].bg, bg, "{scheme:?}");
                        assert_ne!(buffer[(0, at as u16 + 4)].bg, bg, "{scheme:?}");
                    }
                    // Only the title bar and the list are on the info
                    // surface; the blocker below reads like a reply.
                    let row_bg = |y: usize| {
                        let bgs: std::collections::HashSet<_> =
                            (0..width).map(|x| buffer[(x, y as u16)].bg).collect();
                        bgs
                    };
                    let info = theme::style(Role::InfoSurface).bg;
                    assert_eq!(
                        Some(buffer[(0, title as u16)].bg),
                        theme::style(Role::InfoTitle).bg.or(theme::base().bg),
                        "{scheme:?}"
                    );
                    for y in title + 1..top {
                        // Past the selection highlight, the list's own surface.
                        assert_eq!(
                            buffer[(width - 1, y as u16)].bg,
                            info.unwrap_or_default(),
                            "{scheme:?} row {y}"
                        );
                    }
                    for y in top..=at + 4 {
                        assert_eq!(
                            row_bg(y),
                            [theme::base().bg.unwrap_or_default()].into(),
                            "{scheme:?} row {y}"
                        );
                    }
                    // Each half's spare rows are above its content.
                    assert!(rows[..title].iter().all(|r| r.trim().is_empty()), "{out}");
                    assert!(rows[top..at].iter().all(|r| r.trim().is_empty()), "{out}");
                    for y in 0..title as u16 {
                        for x in 0..width {
                            assert_eq!(Some(buffer[(x, y)].bg), theme::base().bg, "{scheme:?}");
                        }
                    }
                    assert_eq!(at, usize::from(height) - 10, "{out}");
                }
            });
        }
    }

    #[test]
    fn the_list_stays_put_whatever_is_selected_and_however_tall_its_answer() {
        let long = (0..60)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        for (width, height) in [(60u16, 24u16), (80, 24), (100, 40)] {
            let now = Instant::now();
            let mut blockers = Blockers::new();
            blockers.poll(SCREEN);
            blockers.on_data(
                SCREEN,
                &list(vec![row(1, 1, "Zed", "short?"), row(2, 2, "Mateo", &long)]),
                now,
            );
            blockers.open().unwrap();
            let top = usize::from(height / 2);
            // The list's rows, less the selection marker.
            let head = |rows: &[String]| -> Vec<String> {
                rows[..top]
                    .iter()
                    .map(|r| r.replacen('▸', " ", 1))
                    .collect()
            };
            let (first, _) = drawn(&mut blockers, width, height, now);
            let out = first.join("\n");
            assert!(first[top - 3].starts_with(" Open blockers"), "{out}");
            assert!(first[top - 2].starts_with("▸ #1 @crew Zed"), "{out}");
            assert!(first[top - 1].starts_with("  #2 @crew Mateo"), "{out}");
            blockers.key(SCREEN, alt(KeyCode::Down), now);
            let (second, _) = drawn(&mut blockers, width, height, now);
            assert!(
                second[top - 1].starts_with("▸ #2 @crew Mateo"),
                "{second:#?}"
            );
            assert_eq!(head(&second), head(&first), "{width}x{height}");
            // A tall draft takes the details' rows, never the list's.
            for _ in 0..8 {
                blockers.key(SCREEN, alt(KeyCode::Enter), now);
            }
            let (tall, _) = drawn(&mut blockers, width, height, now);
            assert_eq!(head(&tall), head(&first), "{tall:#?}");
            // Each half keeps to itself: the bottom ends on the last row.
            for rows in [&first, &second, &tall] {
                assert!(rows[top].trim().is_empty(), "{rows:#?}");
                let action = rows
                    .iter()
                    .position(|r| r.contains("[Cancel]"))
                    .expect("actions");
                assert_eq!(action, usize::from(height) - 2, "{rows:#?}");
            }
        }
    }

    #[test]
    fn the_list_window_moves_only_to_keep_the_selection_in_view() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.on_data(
            SCREEN,
            &list(
                (1..=15)
                    .map(|i| row(i, i, "Zed", &format!("question {i}?")))
                    .collect(),
            ),
            now,
        );
        blockers.open().unwrap();
        // 60x24: a 12-row top half, the title and 11 rows.
        let listed = |rows: &[String]| -> Vec<i64> {
            rows[..12]
                .iter()
                .map(|r| r.chars().skip(2).collect::<String>())
                .filter_map(|r| r.strip_prefix('#')?.split(' ').next()?.parse().ok())
                .collect()
        };
        let (rows, _) = drawn(&mut blockers, 60, 24, now);
        assert_eq!(listed(&rows), (1..=11).collect::<Vec<_>>(), "{rows:#?}");
        for _ in 0..10 {
            blockers.key(SCREEN, alt(KeyCode::Down), now);
        }
        let (rows, _) = drawn(&mut blockers, 60, 24, now);
        assert_eq!(listed(&rows), (1..=11).collect::<Vec<_>>(), "{rows:#?}");
        // Past the foot: one row on.
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        let (rows, _) = drawn(&mut blockers, 60, 24, now);
        assert_eq!(listed(&rows), (2..=12).collect::<Vec<_>>(), "{rows:#?}");
        // Back up inside the window: it stays.
        for _ in 0..5 {
            blockers.key(SCREEN, alt(KeyCode::Up), now);
        }
        let (rows, _) = drawn(&mut blockers, 60, 24, now);
        assert_eq!(listed(&rows), (2..=12).collect::<Vec<_>>(), "{rows:#?}");
    }

    #[test]
    fn an_empty_queue_says_so_at_the_foot_of_the_top_half() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let now = Instant::now();
                let mut blockers = Blockers::new();
                blockers.poll(SCREEN);
                blockers.on_data(SCREEN, &list(Vec::new()), now);
                blockers.open().unwrap();
                let (rows, buffer) = drawn(&mut blockers, 60, 24, now);
                let done = rows
                    .iter()
                    .position(|r| r.contains("No open blockers."))
                    .expect("done");
                // Where the list would end; the bottom half stays empty.
                assert_eq!(done, 24 / 2 - 1, "{rows:#?}");
                let card = theme::style(Role::InfoSurface).bg.unwrap_or_default();
                let base = theme::base().bg.unwrap_or_default();
                assert_eq!(buffer[(59, done as u16)].bg, card, "{scheme:?}");
                for y in done + 1..24 {
                    assert_eq!(buffer[(59, y as u16)].bg, base, "{scheme:?} row {y}");
                    assert_eq!(rows[y].trim(), "", "{rows:#?}");
                }
            });
        }
    }

    #[test]
    fn long_details_keep_to_the_bottom_half_and_page_in_between() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let body = (0..60)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        blockers.on_data(
            SCREEN,
            &list(vec![row(1, 1, "Zed", &body), row(2, 2, "Mateo", "second?")]),
            now,
        );
        blockers.open().unwrap();
        let (rows, _) = drawn(&mut blockers, 60, 24, now);
        let out = rows.join("\n");
        // The list ends the top half; the bottom half starts with one gap.
        assert!(
            rows[10].starts_with("▸ #1") && rows[11].starts_with("  #2"),
            "{out}"
        );
        let at = restated(&rows, "#1 @crew Zed");
        assert_eq!(at, 13, "one blank row, then the details: {out}");
        assert!(rows[at + 1].starts_with("  line 0"), "{out}");
        let label = rows
            .iter()
            .position(|r| r.starts_with("answer → Zed"))
            .expect(&out);
        assert!(rows[label].contains("PgUp/PgDn text 2/119"), "{out}");
        assert_eq!(
            rows[label - 1].trim(),
            "",
            "one gap before the recipient: {out}"
        );
        assert_eq!(rows[label + 1].trim(), "", "{out}");
        assert_eq!(
            label,
            24 - 2 - 4 - 1,
            "the answer box keeps its rows: {out}"
        );
        // Paging moves only the text between.
        blockers.key(SCREEN, press(KeyCode::PageDown), now);
        let (paged, _) = drawn(&mut blockers, 60, 24, now);
        assert!(!paged[at + 1].starts_with("  line 0"), "{paged:#?}");
        assert_eq!(paged[..at + 1], rows[..at + 1]);
        assert!(paged[label].contains("PgUp/PgDn text 3/119"), "{paged:#?}");
        assert_eq!(paged[label + 1..], rows[label + 1..]);
        // A taller window shows more of it, still from where it was.
        let (tall, _) = drawn(&mut blockers, 60, 40, now);
        assert_eq!(restated(&tall, "#1 @crew Zed"), 21);
        // A multiline answer takes rows from the text only.
        let shown = blockers.text_rows.1;
        blockers.key(SCREEN, alt(KeyCode::Enter), now);
        blockers.key(SCREEN, alt(KeyCode::Enter), now);
        let (grown, _) = drawn(&mut blockers, 60, 40, now);
        assert_eq!(blockers.text_rows.1, shown - 2, "{grown:#?}");
        assert_eq!(restated(&grown, "#1 @crew Zed"), 21);
        assert_eq!(grown[..20], tall[..20], "the top half never moves");
    }

    #[test]
    fn a_read_only_row_puts_its_reason_just_above_the_actions() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let mut read_only = row(1, 1, "Zed", "watch only?");
        read_only.can_answer = false;
        read_only.can_cancel = false;
        blockers.on_data(SCREEN, &list(vec![read_only]), now);
        blockers.open().unwrap();
        let (rows, _) = drawn(&mut blockers, 80, 24, now);
        let out = rows.join("\n");
        let at = restated(&rows, "#1 @crew Zed");
        assert!(rows[at + 1].starts_with("  watch only?"), "{out}");
        assert_eq!(rows[at + 2].trim(), "", "{out}");
        assert!(rows[at + 3].starts_with("Read only:"), "{out}");
        assert_eq!(at + 3, 24 - 3, "no answer box: {out}");
    }

    fn focus(blockers: &Blockers) -> Focus {
        blockers.panel.as_ref().unwrap().focus
    }

    fn action(blockers: &Blockers, now: Instant) -> String {
        blockers.lines_below(80, now)[0].to_string()
    }

    #[test]
    fn tab_offers_discussing_the_creator_before_cancel_and_backtab_reverses() {
        let now = Instant::now();
        let mut blockers = open(now);
        assert_eq!(action(&blockers, now), "[discuss with Zed] [Cancel] Tab");
        // One action row, on the card at 60 and 80 columns in every scheme.
        for scheme in crate::ui::theme::testing::SCHEMES {
            crate::ui::theme::testing::with_scheme(scheme, || {
                for width in [60, 80] {
                    let (rows, _) = drawn(&mut blockers, width, 24, now);
                    assert!(
                        rows[22].starts_with("[discuss with Zed] [Cancel] Tab"),
                        "{rows:#?}"
                    );
                }
            });
        }
        let mut seen = Vec::new();
        for _ in 0..3 {
            blockers.key(SCREEN, press(KeyCode::Tab), now);
            seen.push(focus(&blockers));
        }
        assert_eq!(seen, [Focus::Discuss, Focus::Cancel, Focus::Answer]);
        seen.clear();
        for _ in 0..3 {
            blockers.key(SCREEN, press(KeyCode::BackTab), now);
            seen.push(focus(&blockers));
        }
        assert_eq!(seen, [Focus::Cancel, Focus::Discuss, Focus::Answer]);
        // Focused, the button takes the selection colour.
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        let line = &blockers.lines_below(80, now)[0];
        assert_eq!(line.spans[0].style, theme::style(Role::Selection));
        assert_eq!(line.spans[2].style, theme::dim());
        // A human creator has no conversation here; no cancel skips Cancel.
        let chains = |rows: Vec<proto::Blocker>| {
            let mut blockers = Blockers::new();
            blockers.poll(SCREEN);
            blockers.on_data(SCREEN, &list(rows), now);
            blockers.open().unwrap();
            let mut seen = Vec::new();
            for _ in 0..3 {
                blockers.key(SCREEN, press(KeyCode::Tab), now);
                seen.push(focus(&blockers));
            }
            (action(&blockers, now), seen)
        };
        let mut human = row(1, 1, "will", "q?");
        human.creator.as_mut().unwrap().kind = proto::Kind::Human.into();
        assert_eq!(
            chains(vec![human]),
            (
                "[Cancel] Tab".into(),
                vec![Focus::Cancel, Focus::Answer, Focus::Cancel]
            )
        );
        let mut keep = row(1, 1, "Zed", "q?");
        keep.can_cancel = false;
        assert_eq!(
            chains(vec![keep]),
            (
                "[discuss with Zed] Tab".into(),
                vec![Focus::Discuss, Focus::Answer, Focus::Discuss]
            )
        );
    }

    #[test]
    fn a_refresh_keeps_discuss_focused_while_the_creator_is_an_agent() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let mut first = row(1, 1, "Zed", "q?");
        first.can_cancel = false;
        blockers.on_data(SCREEN, &list(vec![first.clone()]), now);
        blockers.open().unwrap();
        typed(&mut blockers, "draft", now);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        assert_eq!(focus(&blockers), Focus::Discuss);
        // The same rows again: still discussing, and Enter discusses.
        blockers.on_data(SCREEN, &list(vec![first.clone()]), now);
        assert_eq!(focus(&blockers), Focus::Discuss);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Discuss { id: 101, name } if name == "Zed"
        ));
        assert!(blockers.chains[&1].pending.is_none(), "no answer sent");
        // A creator that is no longer an agent takes the focus back.
        blockers.open().unwrap();
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        first.creator.as_mut().unwrap().kind = proto::Kind::Human.into();
        blockers.on_data(SCREEN, &list(vec![first]), now);
        assert_eq!(focus(&blockers), Focus::Answer);
    }

    #[test]
    fn a_long_creator_name_shortens_and_the_other_buttons_stay() {
        let now = Instant::now();
        let long = "Bartholomew-the-long-named-agent-of-the-crew";
        for held in [false, true] {
            let mut blockers = Blockers::new();
            blockers.poll(SCREEN);
            blockers.on_data(SCREEN, &list(vec![row(1, 1, long, "q?")]), now);
            blockers.open().unwrap();
            if held {
                blockers.held.push(1);
            }
            blockers.key(SCREEN, press(KeyCode::Tab), now);
            for width in [60u16, 80] {
                let (rows, buffer) = drawn(&mut blockers, width, 24, now);
                let line = rows[22].trim_end();
                let button = if held { "[Dismiss]" } else { "[Cancel]" };
                assert!(line.ends_with(&format!("{button} Tab")), "{line}");
                assert!(line.starts_with("[discuss with Bart"), "{line}");
                assert_eq!(
                    line.contains("…] "),
                    width == 60,
                    "shortened only when needed: {line}"
                );
                assert!(line.width() <= usize::from(width), "{line}");
                // The focused button still shows it.
                if let Some(bg) = theme::style(Role::Selection).bg {
                    assert_eq!(buffer[(1, 22)].bg, bg);
                }
            }
        }
    }

    #[test]
    fn discussing_opens_the_creator_and_never_answers_or_cancels() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "go", now);
        let first = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        // Not confirmed: Enter on the answer would send it again.
        blockers.on_data(
            SCREEN,
            &written(
                &first.request_key,
                Err(SideError::Transient("timeout".into())),
            ),
            now,
        );
        assert!(blockers.chains[&1].pending.as_ref().unwrap().uncertain);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        match blockers.key(SCREEN, press(KeyCode::Enter), now) {
            Outcome::Discuss { id, name } => {
                assert_eq!(
                    (id, name.as_str()),
                    (101, "Zed"),
                    "the creator, not its owner"
                )
            }
            _ => panic!("expected a discussion"),
        }
        // The page stays, as it was, beneath the conversation.
        assert!(blockers.is_open());
        assert!(
            blockers.chains[&1].pending.as_ref().unwrap().uncertain,
            "nothing resent"
        );
        // Just replaced, the guard holds answers but not a discussion.
        blockers.open().unwrap();
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(5, 1, "Zed", "changed?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            now,
        );
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Discuss { .. }
        ));
    }

    #[test]
    fn reopening_returns_to_the_same_blocker_or_the_nearest() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let rows = |ids: &[i64]| {
            list(
                ids.iter()
                    .map(|&root| row(root, root, "Zed", &format!("q{root}?")))
                    .collect(),
            )
        };
        blockers.on_data(SCREEN, &rows(&[1, 2, 3]), now);
        blockers.open().unwrap();
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        typed(&mut blockers, "half", now);
        blockers.key(SCREEN, press(KeyCode::Esc), now);
        let selected = |blockers: &Blockers| blockers.selected().map(|b| (b.root_id, b.id));
        blockers.open().unwrap();
        assert_eq!(selected(&blockers), Some((2, 2)));
        assert_eq!(blockers.chains[&2].answer.draft().text, "half");
        blockers.key(SCREEN, press(KeyCode::Esc), now);
        // A replacement keeps its place: the same root, a new version.
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(1, 1, "Zed", "q1?"),
                row(9, 2, "Zed", "q2 again?"),
                row(3, 3, "Zed", "q3?"),
            ]),
            now,
        );
        blockers.open().unwrap();
        assert_eq!(selected(&blockers), Some((2, 9)));
        blockers.key(SCREEN, press(KeyCode::Esc), now);
        // Gone while closed: the next one along.
        blockers.on_data(SCREEN, &rows(&[1, 3]), now);
        blockers.open().unwrap();
        assert_eq!(selected(&blockers), Some((3, 3)));
        blockers.key(SCREEN, press(KeyCode::Esc), now);
        // Nothing left: done, as before.
        blockers.on_data(SCREEN, &rows(&[]), now);
        blockers.open().unwrap();
        assert!(blockers.done());
    }

    #[test]
    fn polls_one_at_a_time_for_its_own_screen_and_stop_on_an_old_server() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        assert_eq!(
            blockers.poll(SCREEN),
            Some(Request::Blockers { id: SCREEN })
        );
        assert_eq!(blockers.poll(SCREEN), None, "one in flight");
        // Another screen's answer is not this poll's.
        let other = Data::Blockers {
            id: SCREEN + 1,
            result: Ok(Default::default()),
        };
        blockers.on_data(SCREEN, &other, now);
        assert_eq!(blockers.poll(SCREEN), None);
        // A transient failure stays quiet and the next tick asks again.
        let failed = Data::Blockers {
            id: SCREEN,
            result: Err(SideError::Transient("timeout".into())),
        };
        blockers.on_data(SCREEN, &failed, now);
        assert_eq!(blockers.count(), None);
        assert!(blockers.poll(SCREEN).is_some());
        // An old server: no more polls, and F2 says why, until a reload.
        let old = Data::Blockers {
            id: SCREEN,
            result: Err(SideError::Unsupported("no blockers.v1".into())),
        };
        blockers.on_data(SCREEN, &old, now);
        assert_eq!(blockers.poll(SCREEN), None);
        assert!(blockers.open().unwrap_err().contains("blockers-mine.v1"));
        assert!(!blockers.is_open());
        blockers.reload();
        assert!(blockers.poll(SCREEN).is_some());
    }

    #[test]
    fn counts_the_whole_scope_and_notices_only_real_changes_while_open() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        // The first page of 50, of 60 open: the count is the scope's.
        let page: Vec<_> = (1..=50).map(|i| row(i, i, "Zed", "q")).collect();
        blockers.on_data(
            SCREEN,
            &Data::Blockers {
                id: SCREEN,
                result: Ok(Box::new(proto::ListBlockersResponse {
                    blockers: page.clone(),
                    active_count: 60,
                    has_more: true,
                    next_after_root_id: Some(50),
                })),
            },
            now,
        );
        assert_eq!(blockers.count(), Some(60));
        blockers.open().unwrap();
        let text = |b: &Blockers| {
            b.lines_above(80)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(text(&blockers).contains("+ 10 more not shown"));
        // The initial load was no update; an identical poll is none either.
        let status = |b: &Blockers, at| b.status_line(b.selected_chain(), at).to_string();
        assert_eq!(status(&blockers, now), "");
        let of = |rows: &[proto::Blocker], active| Data::Blockers {
            id: SCREEN,
            result: Ok(Box::new(proto::ListBlockersResponse {
                blockers: rows.to_vec(),
                active_count: active,
                ..Default::default()
            })),
        };
        blockers.on_data(SCREEN, &of(&page, 60), now);
        assert_eq!(status(&blockers, now), "");
        // A change shows for five seconds.
        blockers.on_data(SCREEN, &of(&page[1..], 59), now);
        assert_eq!(status(&blockers, now), "Blockers updated.");
        assert_eq!(
            status(&blockers, now + NOTICE - Duration::from_millis(1)),
            "Blockers updated."
        );
        assert_eq!(status(&blockers, now + NOTICE), "");
        assert_eq!(blockers.next_frame(now), Some(now + NOTICE));
    }

    #[test]
    fn a_replaced_blocker_holds_enter_for_a_second_and_keeps_the_draft() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "yes", now);
        // Another agent replaces the selected blocker's text.
        let later = now + Duration::from_secs(3);
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(5, 1, "Zed", "first, revised?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            later,
        );
        assert!(
            blockers
                .lines_above(80)
                .iter()
                .any(|l| l.to_string() == "first, revised?")
        );
        assert_eq!(blockers.next_frame(later), Some(later + GUARD));
        // Enter is swallowed, not queued; editing does not restart the wait.
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), later),
            Outcome::Stay
        ));
        typed(&mut blockers, "!", later + Duration::from_millis(500));
        assert!(matches!(
            blockers.key(
                SCREEN,
                press(KeyCode::Enter),
                later + Duration::from_millis(999)
            ),
            Outcome::Stay
        ));
        assert!(blockers.chains[&1].pending.is_none(), "nothing queued");
        // After the second, Enter answers the new version with the same draft.
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), later + GUARD));
        assert_eq!((req.id, req.answer.as_str()), (5, "yes!"));
        // A poll of the same text starts no new wait.
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(5, 1, "Zed", "first, revised?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            later + GUARD,
        );
        assert_eq!(blockers.chains[&1].guard_until, Some(later + GUARD));
    }

    #[test]
    fn an_uncertain_answer_retries_with_the_same_key_while_others_stay_open() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "go", now);
        let first = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        // While it is out, its draft is fixed and Enter does nothing.
        typed(&mut blockers, "x", now);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Stay
        ));
        // The other blocker answers independently, with its own draft and key.
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        typed(&mut blockers, "no", now);
        let second = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        assert_eq!((second.id, second.answer.as_str()), (2, "no"));
        assert_ne!(first.request_key, second.request_key);
        // No reply for the first: not confirmed, and Enter repeats it exactly.
        blockers.on_data(
            SCREEN,
            &written(
                &first.request_key,
                Err(SideError::Transient("timeout".into())),
            ),
            now,
        );
        blockers.key(SCREEN, alt(KeyCode::Up), now);
        assert_eq!(
            blockers
                .status_line(blockers.selected_chain(), now)
                .to_string(),
            "Not confirmed: Enter retries"
        );
        let retry = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        assert_eq!(retry, first);
        // Replies in either order resolve their own blockers.
        blockers.on_data(
            SCREEN,
            &written(&second.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!(blockers.count(), Some(1));
        assert!(blockers.chains[&1].pending.is_some());
        blockers.on_data(
            SCREEN,
            &written(&first.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!(blockers.count(), None);
        assert!(blockers.chains.is_empty());
    }

    #[test]
    fn a_row_kept_for_its_reply_is_not_counted_twice() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "go", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        // The server resolved it before replying: one open, both shown.
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![row(2, 2, "Mateo", "second?")]), now);
        assert_eq!((blockers.rows.len(), blockers.count()), (2, Some(1)));
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!((blockers.rows.len(), blockers.count()), (1, Some(1)));
    }

    #[test]
    fn a_late_reply_for_an_old_version_leaves_its_replacement_and_draft() {
        for cancel in [false, true] {
            let now = Instant::now();
            let mut blockers = open(now);
            typed(&mut blockers, "go", now);
            let key = if cancel {
                blockers.key(SCREEN, press(KeyCode::BackTab), now);
                blockers.key(SCREEN, press(KeyCode::Enter), now);
                match blockers.key(SCREEN, press(KeyCode::Enter), now) {
                    Outcome::Request(Request::CancelBlocker { req, .. }) => req.request_key,
                    _ => panic!("expected a cancel"),
                }
            } else {
                answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now)).request_key
            };
            // Version 1 resolved; an agent publishes version 5 in the same
            // chain, and the poll shows it before version 1's reply.
            blockers.poll(SCREEN);
            blockers.on_data(
                SCREEN,
                &list(vec![
                    row(5, 1, "Zed", "again?"),
                    row(2, 2, "Mateo", "second?"),
                ]),
                now,
            );
            blockers.on_data(SCREEN, &written(&key, Ok(Default::default())), now);
            assert_eq!(blockers.selected().unwrap().id, 5, "cancel={cancel}");
            assert_eq!(blockers.count(), Some(2));
            let chain = &blockers.chains[&1];
            assert!(chain.pending.is_none() && chain.error.is_none());
            // The delivered answer is not a draft for the new version; an
            // unsent draft survives a cancel.
            let draft = if cancel { "go" } else { "" };
            assert_eq!(chain.answer.draft().text, draft, "cancel={cancel}");
            assert!(!blockers.done());
            // After the guard, a plain Enter sends nothing on the answer's
            // behalf; after a cancel it answers version 5 with the draft.
            if cancel {
                blockers.key(SCREEN, press(KeyCode::Tab), now + GUARD);
            }
            let after = blockers.key(SCREEN, press(KeyCode::Enter), now + GUARD);
            if cancel {
                let req = answer_request(after);
                assert_eq!((req.id, req.answer.as_str()), (5, "go"));
            } else {
                assert!(matches!(after, Outcome::Stay));
            }
        }
    }

    #[test]
    fn a_replacement_withdraws_a_pending_cancel_confirmation() {
        let now = Instant::now();
        let mut blockers = open(now);
        blockers.key(SCREEN, press(KeyCode::BackTab), now);
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Confirm);
        // Version 1 is replaced by 5 while the question is up.
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(5, 1, "Zed", "changed?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            now,
        );
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Cancel);
        // An immediate Enter neither confirms nor asks again.
        for _ in 0..2 {
            assert!(matches!(
                blockers.key(SCREEN, press(KeyCode::Enter), now),
                Outcome::Stay
            ));
        }
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Cancel);
        // After the second, cancelling the new version takes two Enters.
        let later = now + GUARD;
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), later),
            Outcome::Stay
        ));
        match blockers.key(SCREEN, press(KeyCode::Enter), later) {
            Outcome::Request(Request::CancelBlocker { req, .. }) => assert_eq!(req.id, 5),
            _ => panic!("expected a cancel of the new version"),
        }
    }

    #[test]
    fn done_waits_while_blockers_past_the_first_page_are_open() {
        let now = Instant::now();
        let mut blockers = open(now);
        blockers.active = 60;
        for _ in 0..2 {
            typed(&mut blockers, "ok", now);
            let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
            blockers.on_data(
                SCREEN,
                &written(&req.request_key, Ok(Default::default())),
                now,
            );
        }
        assert_eq!(blockers.count(), Some(58));
        assert!(!blockers.done());
        let text: Vec<String> = blockers
            .lines_above(80)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.iter().any(|l| l == "Loading more…"), "{text:?}");
        // The next poll refills the panel.
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![row(3, 3, "Zed", "third?")]), now);
        assert_eq!(blockers.selected().unwrap().id, 3);
    }

    #[test]
    fn losing_cancel_permission_withdraws_the_confirmation() {
        let now = Instant::now();
        let mut blockers = open(now);
        blockers.key(SCREEN, press(KeyCode::BackTab), now);
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        let mut first = row(1, 1, "Zed", "first?");
        first.can_cancel = false;
        first.can_answer = false;
        blockers.on_data(
            SCREEN,
            &list(vec![first, row(2, 2, "Mateo", "second?")]),
            now,
        );
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Answer);
        for _ in 0..3 {
            assert!(matches!(
                blockers.key(SCREEN, press(KeyCode::Enter), now),
                Outcome::Stay
            ));
        }
        assert!(blockers.chains[&1].pending.is_none());
        // Tab cannot reach Cancel either, and Enter rechecks it if it did.
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Discuss);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        assert_eq!(blockers.panel.as_ref().unwrap().focus, Focus::Answer);
        blockers.panel.as_mut().unwrap().focus = Focus::Confirm;
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Stay
        ));
        assert!(blockers.chains[&1].pending.is_none());
    }

    #[test]
    fn a_refusal_stays_with_its_draft_across_polls_until_dismissed() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "valuable answer", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        let others = list(vec![row(2, 2, "Mateo", "second?")]);
        blockers.on_data(SCREEN, &others, now);
        blockers.on_data(
            SCREEN,
            &written(
                &req.request_key,
                Err(SideError::Refused("another human answered first".into())),
            ),
            now,
        );
        for at in 1..3 {
            blockers.on_data(SCREEN, &others, now + Duration::from_secs(at));
        }
        assert_eq!(blockers.chains[&1].answer.draft().text, "valuable answer");
        assert_eq!(blockers.selected().unwrap().root_id, 1);
        assert_eq!(blockers.count(), Some(1), "the server's count");
        assert!(
            blockers
                .lines_above(80)
                .iter()
                .any(|l| l.to_string().starts_with("No longer open"))
        );
        assert_eq!(
            blockers
                .status_line(blockers.selected_chain(), now)
                .to_string(),
            "another human answered first"
        );
        assert!(blockers.answer_height(80) > 0, "the draft shows");
        // Enter cannot answer it again; Tab and Enter dismiss it.
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Stay
        ));
        assert!(blockers.chains[&1].pending.is_none());
        blockers.key(SCREEN, press(KeyCode::BackTab), now);
        assert!(
            blockers
                .lines_below(80, now)
                .iter()
                .any(|l| l.to_string().contains("[Dismiss]"))
        );
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        assert!(!blockers.chains.contains_key(&1));
        assert_eq!(blockers.selected().unwrap().root_id, 2);
        // With the last one gone too, the goodbye can start.
        blockers.on_data(SCREEN, &list(vec![]), now);
        assert!(blockers.done());
    }

    #[test]
    fn a_kept_refusal_does_not_renew_the_update_notice() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "go", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        let others = list(vec![row(2, 2, "Mateo", "second?")]);
        blockers.on_data(SCREEN, &others, now);
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Err(SideError::Refused("taken".into()))),
            now,
        );
        // The other blocker is selected while the refusal stays listed.
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        assert_eq!(blockers.selected().unwrap().root_id, 2);
        let status = |b: &Blockers, at| b.status_line(b.selected_chain(), at).to_string();
        // Identical polls each second: the notice from the change ends.
        for at in 1..=10 {
            blockers.on_data(SCREEN, &others, now + Duration::from_secs(at));
        }
        assert_eq!(status(&blockers, now + Duration::from_secs(10)), "");
        assert_eq!(blockers.next_frame(now + Duration::from_secs(10)), None);
        assert!(
            blockers.chains.contains_key(&1),
            "the refusal is still kept"
        );
    }

    #[test]
    fn only_so_many_refusals_are_kept() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let all: Vec<_> = (1..=25).map(|i| row(i, i, "Zed", "q?")).collect();
        blockers.on_data(SCREEN, &list(all), now);
        blockers.open().unwrap();
        for _ in 1..=25 {
            typed(&mut blockers, "a", now);
            let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
            blockers.on_data(
                SCREEN,
                &written(&req.request_key, Err(SideError::Refused("no".into()))),
                now,
            );
            blockers.key(SCREEN, alt(KeyCode::Down), now);
        }
        blockers.on_data(SCREEN, &list(vec![]), now);
        assert_eq!(blockers.rows.len(), KEPT_REFUSALS);
        assert_eq!(blockers.rows[0].root_id, 6, "the oldest went first");
        assert_eq!(blockers.chains.len(), KEPT_REFUSALS);
    }

    #[test]
    fn a_draft_stays_in_view_read_only_when_answering_is_no_longer_allowed() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "half written", now);
        let mut first = row(1, 1, "Zed", "first?");
        first.can_answer = false;
        blockers.on_data(
            SCREEN,
            &list(vec![first, row(2, 2, "Mateo", "second?")]),
            now,
        );
        assert!(blockers.answer_height(80) > 0, "the draft still shows");
        typed(&mut blockers, "x", now);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Stay
        ));
        assert_eq!(blockers.chains[&1].answer.draft().text, "half written");
        assert!(blockers.chains[&1].pending.is_none());
        // With no draft, a read-only row has no answer box.
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        let mut second = row(2, 2, "Mateo", "second?");
        second.can_answer = false;
        let mut first = row(1, 1, "Zed", "first?");
        first.can_answer = false;
        blockers.on_data(SCREEN, &list(vec![first, second]), now);
        assert_eq!(blockers.answer_height(80), 0);
    }

    #[test]
    fn the_title_is_the_same_whatever_is_selected() {
        let now = Instant::now();
        let mut blockers = open(now);
        let title = |b: &Blockers, width| b.title(width).to_string();
        let wide = title(&blockers, 100);
        assert!(wide.starts_with(" Open blockers "), "{wide:?}");
        assert!(wide.ends_with(" Alt-↑/↓ · Tab · Esc "), "{wide:?}");
        assert_eq!(wide.width(), 100);
        // Narrow: the way out still shows; narrower still, only the title.
        assert!(title(&blockers, 30).ends_with(" Esc "));
        assert!(title(&blockers, 14).width() <= 14);
        assert_eq!(
            blockers.title(80).style,
            theme::style(Role::InfoTitle),
            "the whole bar"
        );
        // Another selection, a read-only row, a kept refusal: the same bar.
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        assert_eq!(title(&blockers, 100), wide);
        let mut second = row(2, 2, "Mateo", "second?");
        second.can_answer = false;
        blockers.on_data(SCREEN, &list(vec![row(1, 1, "Zed", "first?"), second]), now);
        assert_eq!(title(&blockers, 100), wide);
        blockers.key(SCREEN, alt(KeyCode::Up), now);
        typed(&mut blockers, "go", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        blockers.on_data(SCREEN, &list(vec![row(2, 2, "Mateo", "second?")]), now);
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Err(SideError::Refused("taken".into()))),
            now,
        );
        assert_eq!(title(&blockers, 100), wide);
        // What the title no longer says, the blocker's own rows do.
        assert!(
            blockers
                .lines_above(100)
                .iter()
                .any(|l| l.to_string().starts_with("No longer open"))
        );
    }

    #[test]
    fn read_only_says_whether_the_creator_or_ownership_is_missing() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let mut retired = row(1, 1, "Zed", "q?");
        retired.can_answer = false;
        let mut theirs = row(2, 2, "Mateo", "q?");
        theirs.can_answer = false;
        theirs.can_cancel = false;
        blockers.on_data(SCREEN, &list(vec![retired, theirs]), now);
        blockers.open().unwrap();
        let label = |b: &Blockers| b.lines_above(200).last().unwrap().to_string();
        assert_eq!(
            label(&blockers),
            "Can't answer: Zed is no longer available; it can still be cancelled."
        );
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        assert_eq!(
            label(&blockers),
            "Read only: answering needs an agent of yours in @crew."
        );
    }

    #[test]
    fn a_refused_answer_keeps_the_draft_and_the_next_try_has_a_new_key() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "go", now);
        let first = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        blockers.on_data(
            SCREEN,
            &written(
                &first.request_key,
                Err(SideError::Refused("blocker #1 is not open".into())),
            ),
            now,
        );
        assert_eq!(blockers.chains[&1].answer.draft().text, "go");
        assert_eq!(
            blockers
                .status_line(blockers.selected_chain(), now)
                .to_string(),
            "blocker #1 is not open"
        );
        // The panel stays: an error needs attention before any goodbye.
        assert!(!blockers.done());
        let again = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        assert_eq!(again.answer, "go");
        assert_ne!(again.request_key, first.request_key);
    }

    #[test]
    fn cancelling_asks_first_and_esc_returns_to_the_same_draft() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "half", now);
        blockers.key(SCREEN, press(KeyCode::BackTab), now);
        // Typing on the action types nothing.
        typed(&mut blockers, "zz", now);
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        assert!(
            blockers
                .lines_below(80, now)
                .iter()
                .any(|l| l.to_string().contains("Cancel this blocker?"))
        );
        // Esc steps back to the answer, not out of the panel.
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Esc), now),
            Outcome::Stay
        ));
        assert!(blockers.is_open());
        assert_eq!(blockers.chains[&1].answer.draft().text, "half");
        blockers.key(SCREEN, press(KeyCode::BackTab), now);
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        match blockers.key(SCREEN, press(KeyCode::Enter), now) {
            Outcome::Request(Request::CancelBlocker { id, req }) => {
                assert_eq!((id, req.id), (SCREEN, 1));
                assert!(!req.request_key.is_empty());
            }
            _ => panic!("expected a cancel"),
        }
        // Then Esc leaves the panel.
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Esc), now),
            Outcome::Close
        ));
        assert!(!blockers.is_open());
    }

    #[test]
    fn read_only_rows_say_why_and_take_no_answer() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        let mut theirs = row(1, 1, "Zed", "q?");
        theirs.can_answer = false;
        theirs.can_cancel = false;
        blockers.on_data(SCREEN, &list(vec![theirs]), now);
        blockers.open().unwrap();
        typed(&mut blockers, "hi", now);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Enter), now),
            Outcome::Stay
        ));
        assert_eq!(blockers.chains[&1].answer.draft().text, "");
        assert!(
            blockers
                .lines_above(80)
                .iter()
                .any(|l| l.to_string().starts_with("Read only:"))
        );
        assert_eq!(blockers.answer_height(80), 0);
    }

    #[test]
    fn the_last_answer_says_so_and_stays_until_esc() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![row(1, 1, "Zed", "q?")]), now);
        blockers.open().unwrap();
        typed(&mut blockers, "ok", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        // Pending: still shown and no goodbye, even when a poll no longer
        // lists it.
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![]), now);
        assert!(!blockers.done());
        assert_eq!(blockers.selected().unwrap().id, 1);
        assert_eq!(blockers.active, 0, "the server's count, not counted again");
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!(blockers.active, 0);
        assert!(blockers.done());
        // Nothing to answer: typing does nothing.
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Char('x')), now),
            Outcome::Stay
        ));
        // A new blocker is offered in place.
        blockers.on_data(SCREEN, &list(vec![row(3, 3, "Zed", "new?")]), now);
        assert!(!blockers.done());
        assert_eq!(blockers.selected().unwrap().id, 3);
        // Answered too: it says so and stays, with no timer, until Esc.
        typed(&mut blockers, "y", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert!(blockers.is_open() && blockers.done());
        assert!(
            blockers
                .lines_above(80)
                .iter()
                .any(|l| l.to_string().contains("No open blockers."))
        );
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Esc), now),
            Outcome::Close
        ));
        assert!(!blockers.is_open());
    }

    #[test]
    fn an_open_card_explains_an_old_server_and_never_says_done() {
        let now = Instant::now();
        let unsupported = || Data::Blockers {
            id: SCREEN,
            result: Err(SideError::Unsupported("no blockers-mine.v1".into())),
        };
        let text = |b: &Blockers| {
            b.lines_above(80)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Opened before the first reply: Loading, then the reason.
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.open().unwrap();
        assert!(text(&blockers).contains("Loading…"));
        blockers.on_data(SCREEN, &unsupported(), now);
        let shown = text(&blockers);
        assert!(
            shown.contains("blockers-mine.v1") && !shown.contains("Loading"),
            "{shown}"
        );
        assert!(!blockers.done());
        assert_eq!(blockers.poll(SCREEN), None);
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Esc), now),
            Outcome::Close
        ));
        // Open on a supported queue that then loses support: the same.
        let mut blockers = open(now);
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &unsupported(), now);
        assert!(text(&blockers).contains("blockers-mine.v1"));
        assert!(!blockers.done());
        assert_eq!(blockers.count(), None);
        // A goodbye already showing gives way to the reason.
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![]), now);
        blockers.open().unwrap();
        assert!(blockers.done());
        blockers.on_data(SCREEN, &unsupported(), now);
        assert!(!blockers.done() && text(&blockers).contains("blockers-mine.v1"));
        // An answer confirmed after support went away does not say Done.
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![row(1, 1, "Zed", "q?")]), now);
        blockers.open().unwrap();
        typed(&mut blockers, "ok", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        blockers.on_data(SCREEN, &unsupported(), now);
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert!(!blockers.done(), "no goodbye for an upgrade");
        assert!(text(&blockers).contains("blockers-mine.v1"));
    }

    #[test]
    fn an_empty_queue_says_done_on_entry() {
        let now = Instant::now();
        let mut blockers = Blockers::new();
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![]), now);
        blockers.open().unwrap();
        assert!(blockers.done());
    }

    #[test]
    fn the_selection_moves_to_the_nearest_survivor() {
        assert_eq!(nearest(Some(2), &[1, 2, 3], &[1, 3]), Some(3));
        assert_eq!(nearest(Some(3), &[1, 2, 3], &[1, 2]), Some(2));
        assert_eq!(nearest(Some(2), &[1, 2, 3], &[1, 2, 3]), Some(2));
        assert_eq!(nearest(Some(1), &[1], &[4]), Some(4));
        assert_eq!(nearest(None, &[], &[]), None);
        // Others' drafts survive a neighbour's removal.
        let now = Instant::now();
        let mut blockers = open(now);
        blockers.key(SCREEN, alt(KeyCode::Down), now);
        typed(&mut blockers, "mine", now);
        blockers.on_data(SCREEN, &list(vec![row(2, 2, "Mateo", "second?")]), now);
        assert_eq!(blockers.selected().unwrap().root_id, 2);
        assert_eq!(blockers.chains[&2].answer.draft().text, "mine");
        assert!(!blockers.chains.contains_key(&1));
    }
}
