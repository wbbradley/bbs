//! Blockers (blockers.v1) for a conversation: the scope's open blockers,
//! polled once a second, and the F2 panel that answers or cancels them in
//! the composer's place. Answer drafts live only in this session; each chain
//! has at most one write in flight, retried only with its own key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::composer::Composer;
use super::event::{BlockerScope, Data, Request, SideError};
use super::theme::{self, Role};
use crate::proto;

/// After the last blocker goes, how long `Done with blockers.` shows.
pub const DONE: Duration = Duration::from_millis(750);
/// After a blocker's text changes under the reader, Enter waits this long.
pub const GUARD: Duration = Duration::from_secs(1);
/// How long an update notice stays.
pub const NOTICE: Duration = Duration::from_secs(5);
/// Blockers listed above the selected one's text.
const LIST_ROWS: usize = 4;

#[derive(Debug, PartialEq)]
enum Support {
    Unknown,
    Supported,
    /// The server lacks blockers.v1; never asked again on this screen.
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
    Cancel,
    Confirm,
}

struct Panel {
    selected: Option<i64>,
    focus: Focus,
    /// Rows of the selected blocker's text scrolled past.
    scroll: u16,
    /// `Done with blockers.` until then; input is consumed meanwhile.
    done_at: Option<Instant>,
}

/// What a key did in the open panel.
pub enum Outcome {
    Stay,
    /// The panel closed: back to the composer.
    Close,
    Request(Request),
    /// Not the panel's key (Ctrl-C): let the keymap have it.
    Pass,
}

pub struct Blockers {
    scope: BlockerScope,
    support: Support,
    polling: bool,
    loaded: bool,
    rows: Vec<proto::Blocker>,
    /// Rows kept for a write still out, which the server no longer counts.
    held: Vec<i64>,
    active: u64,
    poll_error: Option<String>,
    /// The selected text's wrapped rows and the rows it had, at the last draw.
    text_rows: (u16, u16),
    chains: HashMap<i64, Chain>,
    panel: Option<Panel>,
    notice_until: Option<Instant>,
}

impl Blockers {
    pub fn new(scope: BlockerScope) -> Blockers {
        Blockers {
            scope,
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
            notice_until: None,
        }
    }

    /// The next poll for the screen `id`: one at a time, never once the
    /// server has refused the capability.
    pub fn poll(&mut self, id: u64) -> Option<Request> {
        if self.support == Support::Unsupported || self.polling {
            return None;
        }
        self.polling = true;
        Some(Request::Blockers {
            id,
            scope: self.scope.clone(),
        })
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

    pub fn is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// F2: open the panel, or say why not.
    pub fn open(&mut self, now: Instant) -> Result<(), String> {
        if self.support == Support::Unsupported {
            return Err("Blockers need a newer server (blockers.v1); r retries.".into());
        }
        self.panel = Some(Panel {
            selected: self.rows.first().map(|b| b.root_id),
            focus: Focus::Answer,
            scroll: 0,
            done_at: None,
        });
        self.settle_done(now);
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
            } if *to == id => self.written(key, result, now),
            _ => {}
        }
    }

    fn listed(&mut self, list: &proto::ListBlockersResponse, now: Instant) {
        let shown = |rows: &[proto::Blocker]| -> Vec<(i64, String, bool, bool)> {
            rows.iter()
                .map(|b| (b.id, b.body.clone(), b.can_answer, b.can_cancel))
                .collect()
        };
        let changed = self.loaded
            && (shown(&self.rows) != shown(&list.blockers) || self.active != list.active_count);
        let old: Vec<i64> = self.rows.iter().map(|b| b.root_id).collect();
        self.support = Support::Supported;
        self.poll_error = None;
        // A row with a write still out stays until its reply, even when the
        // server already lists it resolved.
        let mut rows = list.blockers.clone();
        self.held.clear();
        for row in &self.rows {
            let pending = self
                .chains
                .get(&row.root_id)
                .is_some_and(|c| c.pending.is_some());
            if pending && !rows.iter().any(|b| b.root_id == row.root_id) {
                rows.push(row.clone());
                self.held.push(row.root_id);
            }
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
            if !self.rows.is_empty() {
                // Something new or still open: stay.
                panel.done_at = None;
            }
        }
        self.settle_done(now);
    }

    fn written(
        &mut self,
        key: &str,
        result: &Result<Box<proto::BlockerMutationResponse>, SideError>,
        now: Instant,
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
                    // A newer version of the chain came first: it stays,
                    // with its draft, for the next poll to settle.
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
                self.settle_done(now);
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

    /// With nothing left to do, start the short goodbye.
    fn settle_done(&mut self, now: Instant) {
        let busy = self
            .chains
            .values()
            .any(|c| c.pending.is_some() || c.error.is_some());
        if let Some(panel) = &mut self.panel
            && self.loaded
            && self.rows.is_empty()
            && !busy
            && panel.done_at.is_none()
        {
            panel.done_at = Some(now + DONE);
        }
    }

    /// Close the panel once its goodbye has shown; true if it closed now.
    pub fn settle(&mut self, now: Instant) -> bool {
        if self
            .panel
            .as_ref()
            .and_then(|p| p.done_at)
            .is_some_and(|at| now >= at)
        {
            self.panel = None;
            return true;
        }
        false
    }

    /// When the panel next changes by itself.
    pub fn next_frame(&self, now: Instant) -> Option<Instant> {
        let panel = self.panel.as_ref()?;
        let guard = self.selected_chain().and_then(|c| c.guard_until);
        let spinning = self
            .chains
            .values()
            .any(|c| c.pending.as_ref().is_some_and(|p| !p.uncertain));
        [
            panel.done_at,
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Outcome::Pass;
        }
        let Some(panel) = &mut self.panel else {
            return Outcome::Pass;
        };
        if panel.done_at.is_some() {
            // The goodbye swallows typing rather than leave it for mail.
            return Outcome::Stay;
        }
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc if panel.focus == Focus::Confirm => {
                panel.focus = Focus::Answer;
                return Outcome::Stay;
            }
            KeyCode::Esc => {
                self.panel = None;
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
        let panel = self.panel.as_mut().expect("open");
        let chain = self.chains.entry(row.root_id).or_insert_with(Chain::new);
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                panel.focus = match panel.focus {
                    Focus::Answer if row.can_cancel => Focus::Cancel,
                    _ => Focus::Answer,
                };
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
                    Focus::Cancel => {
                        panel.focus = Focus::Confirm;
                        Outcome::Stay
                    }
                    Focus::Confirm => {
                        panel.focus = Focus::Cancel;
                        submit(id, chain, Write::Cancel { id: row.id }, now)
                    }
                    Focus::Answer => {
                        if !row.can_answer {
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

    /// The panel's height for this width, before the caller's cap.
    pub fn height(&self, width: u16) -> u16 {
        let (head, body, label) = self.parts(width);
        let body = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .line_count(width);
        (head.len() + body + label.iter().count()) as u16 + self.answer_height(width) + 2
    }

    fn answer_height(&self, width: u16) -> u16 {
        match (self.selected(), self.selected_chain()) {
            (Some(row), Some(chain)) if row.can_answer => chain.answer.height(width, 6),
            _ => 0,
        }
    }

    /// The header, list, and the selected blocker's text and recipient.
    #[cfg(test)]
    fn lines_above(&self, width: u16) -> Vec<Line<'static>> {
        let (mut lines, body, label) = self.parts(width);
        lines.extend(body);
        lines.extend(label);
        lines
    }

    /// The header and list; the selected blocker's text, which gets what
    /// room is left; and the line naming who an answer goes to.
    fn parts(
        &self,
        width: u16,
    ) -> (
        Vec<Line<'static>>,
        Vec<Line<'static>>,
        Option<Line<'static>>,
    ) {
        let width = usize::from(width);
        let Some(panel) = &self.panel else {
            return Default::default();
        };
        if panel.done_at.is_some() {
            let done = Line::styled("Done with blockers.", theme::style(Role::Success));
            return (vec![done], Vec::new(), None);
        }
        let mut lines = vec![theme::clip_line(
            Line::from(vec![
                Span::styled("Blockers", theme::style(Role::Heading)),
                Span::styled(
                    format!(" · {} open · Alt-↑/↓ select · Tab · Esc back", self.active),
                    theme::dim(),
                ),
            ]),
            width,
        )];
        if !self.loaded {
            lines.push(Line::styled("Loading…", theme::dim()));
            return (lines, Vec::new(), None);
        }
        let at = panel
            .selected
            .and_then(|r| self.rows.iter().position(|b| b.root_id == r))
            .unwrap_or(0);
        let start = at.saturating_sub(LIST_ROWS - 1);
        for row in self.rows.iter().skip(start).take(LIST_ROWS) {
            let chosen = panel.selected == Some(row.root_id);
            let mut line = Line::from(vec![
                Span::raw(if chosen { "▸ " } else { "  " }),
                Span::styled(format!("@{}", row.group), theme::style(Role::Group)),
                Span::raw(" "),
                Span::styled(creator(row), theme::style(Role::Agent)),
                Span::raw("  "),
                Span::raw(row.body.lines().next().unwrap_or_default().to_string()),
            ]);
            if chosen {
                line = line.style(theme::style(Role::Selection));
            }
            lines.push(theme::clip_line(line, width));
        }
        let more = self.active.saturating_sub(self.rows.len() as u64);
        if more > 0 {
            lines.push(Line::styled(
                format!("  + {more} more not shown"),
                theme::dim(),
            ));
        }
        let Some(row) = self.selected() else {
            return (lines, Vec::new(), None);
        };
        let mut body = vec![Line::default()];
        body.extend(row.body.lines().map(|text| Line::raw(format!("  {text}"))));
        let label = if row.can_answer {
            Line::from(vec![
                Span::styled("answer → ", theme::dim()),
                Span::styled(creator(row), theme::style(Role::Agent)),
                Span::raw(" "),
                Span::styled(format!("@{}", row.group), theme::style(Role::Group)),
            ])
        } else {
            Line::styled(
                format!(
                    "Read only: answering needs an agent of yours in @{}.",
                    row.group
                ),
                theme::dim(),
            )
        };
        (lines, body, Some(theme::clip_line(label, width)))
    }

    /// The Cancel action, or its confirmation, and the state line.
    fn lines_below(&self, now: Instant) -> Vec<Line<'static>> {
        let (Some(panel), Some(row), Some(chain)) =
            (&self.panel, self.selected(), self.selected_chain())
        else {
            return vec![Line::default(), self.status_line(None, now)];
        };
        let action = match panel.focus {
            Focus::Confirm => Line::from(vec![
                Span::styled("Cancel this blocker? ", theme::style(Role::Warning)),
                Span::styled("Enter confirms · Esc returns", theme::dim()),
            ]),
            focus if row.can_cancel => {
                let style = if focus == Focus::Cancel {
                    theme::style(Role::Selection)
                } else {
                    theme::dim()
                };
                Line::from(vec![
                    Span::styled("[Cancel blocker]", style),
                    Span::styled(" Tab", theme::dim()),
                ])
            }
            _ => Line::default(),
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, now: Instant) {
        let (head, body, label) = self.parts(area.width);
        let below = self.lines_below(now);
        let answer = self.answer_height(area.width);
        let [head_area, body_area, label_area, middle, bottom] = Layout::vertical([
            Constraint::Length(head.len() as u16),
            Constraint::Min(0),
            Constraint::Length(label.iter().count() as u16),
            Constraint::Length(answer),
            Constraint::Length(below.len() as u16),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(head), head_area);
        let body = Paragraph::new(body).wrap(Wrap { trim: false });
        let rows = body.line_count(area.width) as u16;
        self.text_rows = (rows, body_area.height);
        let mut label = label;
        if let Some(panel) = &mut self.panel {
            panel.scroll = panel.scroll.min(rows.saturating_sub(body_area.height));
            if rows > body_area.height
                && let Some(label) = &mut label
            {
                // The rest is a page key away.
                let last = (panel.scroll + body_area.height).min(rows);
                label.spans.push(Span::styled(
                    format!("  PgUp/PgDn text {last}/{rows}"),
                    theme::dim(),
                ));
            }
            frame.render_widget(body.scroll((panel.scroll, 0)), body_area);
        }
        frame.render_widget(Paragraph::new(Vec::from_iter(label)), label_area);
        frame.render_widget(Paragraph::new(below), bottom);
        let focused = self.panel.as_ref().map(|p| p.focus) == Some(Focus::Answer);
        let Some(root) = self.selected().map(|b| b.root_id) else {
            return;
        };
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
    use crate::connect::event::BlockerScope;

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
        let mut blockers = Blockers::new(BlockerScope::Agent("Mateo".into()));
        assert!(blockers.poll(SCREEN).is_some());
        blockers.on_data(
            SCREEN,
            &list(vec![
                row(1, 1, "Zed", "first?"),
                row(2, 2, "Mateo", "second?"),
            ]),
            now,
        );
        blockers.open(now).unwrap();
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

    #[test]
    fn polls_one_at_a_time_for_its_own_screen_and_stop_on_an_old_server() {
        let now = Instant::now();
        let mut blockers = Blockers::new(BlockerScope::Group("crew".into()));
        assert_eq!(
            blockers.poll(SCREEN),
            Some(Request::Blockers {
                id: SCREEN,
                scope: BlockerScope::Group("crew".into()),
            })
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
        assert!(blockers.open(now).unwrap_err().contains("blockers.v1"));
        assert!(!blockers.is_open());
        blockers.reload();
        assert!(blockers.poll(SCREEN).is_some());
    }

    #[test]
    fn counts_the_whole_scope_and_notices_only_real_changes_while_open() {
        let now = Instant::now();
        let mut blockers = Blockers::new(BlockerScope::Agent("Mateo".into()));
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
        blockers.open(now).unwrap();
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
                .any(|l| l.to_string() == "  first, revised?")
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
                blockers.key(SCREEN, press(KeyCode::Tab), now);
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
            assert_eq!(chain.answer.draft().text, "go", "the draft stays");
            assert!(blockers.panel.as_ref().unwrap().done_at.is_none());
        }
    }

    #[test]
    fn a_replacement_withdraws_a_pending_cancel_confirmation() {
        let now = Instant::now();
        let mut blockers = open(now);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
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
        assert!(blockers.panel.as_ref().unwrap().done_at.is_none());
        let again = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        assert_eq!(again.answer, "go");
        assert_ne!(again.request_key, first.request_key);
    }

    #[test]
    fn cancelling_asks_first_and_esc_returns_to_the_same_draft() {
        let now = Instant::now();
        let mut blockers = open(now);
        typed(&mut blockers, "half", now);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
        // Typing on the action types nothing.
        typed(&mut blockers, "zz", now);
        blockers.key(SCREEN, press(KeyCode::Enter), now);
        assert!(
            blockers
                .lines_below(now)
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
        let mut blockers = Blockers::new(BlockerScope::Group("crew".into()));
        blockers.poll(SCREEN);
        let mut theirs = row(1, 1, "Zed", "q?");
        theirs.can_answer = false;
        theirs.can_cancel = false;
        blockers.on_data(SCREEN, &list(vec![theirs]), now);
        blockers.open(now).unwrap();
        typed(&mut blockers, "hi", now);
        blockers.key(SCREEN, press(KeyCode::Tab), now);
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
    fn the_last_answer_says_done_swallows_keys_and_a_new_blocker_keeps_the_panel() {
        let now = Instant::now();
        let mut blockers = Blockers::new(BlockerScope::Agent("Mateo".into()));
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![row(1, 1, "Zed", "q?")]), now);
        blockers.open(now).unwrap();
        typed(&mut blockers, "ok", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        // Pending: still shown and no goodbye, even when a poll no longer
        // lists it.
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![]), now);
        assert!(blockers.panel.as_ref().unwrap().done_at.is_none());
        assert_eq!(blockers.selected().unwrap().id, 1);
        assert_eq!(blockers.active, 0, "the server's count, not counted again");
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!(blockers.active, 0);
        assert_eq!(blockers.panel.as_ref().unwrap().done_at, Some(now + DONE));
        assert_eq!(
            blockers.lines_above(80)[0].to_string(),
            "Done with blockers."
        );
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Char('x')), now),
            Outcome::Stay
        ));
        assert!(matches!(
            blockers.key(SCREEN, press(KeyCode::Esc), now),
            Outcome::Stay
        ));
        // A new blocker during the goodbye keeps the panel open.
        blockers.on_data(SCREEN, &list(vec![row(3, 3, "Zed", "new?")]), now);
        assert!(!blockers.settle(now + DONE));
        assert!(blockers.is_open());
        assert_eq!(blockers.selected().unwrap().id, 3);
        // Answered too: the goodbye runs its 750 ms, then the panel closes.
        typed(&mut blockers, "y", now);
        let req = answer_request(blockers.key(SCREEN, press(KeyCode::Enter), now));
        blockers.on_data(
            SCREEN,
            &written(&req.request_key, Ok(Default::default())),
            now,
        );
        assert_eq!(blockers.next_frame(now), Some(now + DONE));
        assert!(!blockers.settle(now + DONE - Duration::from_millis(1)));
        assert!(blockers.settle(now + DONE));
        assert!(!blockers.is_open());
    }

    #[test]
    fn an_empty_queue_says_done_on_entry() {
        let now = Instant::now();
        let mut blockers = Blockers::new(BlockerScope::Agent("Mateo".into()));
        blockers.poll(SCREEN);
        blockers.on_data(SCREEN, &list(vec![]), now);
        blockers.open(now).unwrap();
        assert_eq!(
            blockers.lines_above(80)[0].to_string(),
            "Done with blockers."
        );
        assert!(blockers.settle(now + DONE));
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
