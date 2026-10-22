//! Local launcher data. Nothing here opens a backend or creates requests.

use std::collections::{BTreeMap, HashSet};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui_textarea::TextArea;

use super::event::Data;
use super::theme::{self, Role};
use crate::proto;
use crate::ui::message::fold;

const MAX_MESSAGES: usize = 10_000;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 50;
const MAX_QUERY_CHARS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Agent(String),
    Group(String),
    Message(i64),
}

impl Target {
    fn category(&self) -> u8 {
        match self {
            Self::Agent(_) => 0,
            Self::Group(_) => 1,
            Self::Message(_) => 2,
        }
    }

    fn message_id(&self) -> i64 {
        match self {
            Self::Message(id) => *id,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub target: Target,
    pub text: String,
    rank: Rank,
}

/// Previously received data, bounded for this client session only.
#[derive(Default)]
pub struct Cache {
    agents: BTreeMap<String, String>,
    groups: BTreeMap<String, String>,
    retired: HashSet<String>,
    messages: BTreeMap<i64, proto::Message>,
    text_bytes: usize,
    revision: u64,
}

impl Cache {
    pub fn observe(&mut self, data: &Data) {
        match data {
            Data::Agents(Ok(list)) => {
                self.revision = self.revision.wrapping_add(1);
                self.agents.clear();
                self.retired = list
                    .agents
                    .iter()
                    .filter(|a| a.retired)
                    .map(|a| a.name.to_lowercase())
                    .collect();
                // A retired name may have a new, live identity.
                for agent in list.agents.iter().filter(|a| !a.retired) {
                    self.retired.remove(&agent.name.to_lowercase());
                    self.agent(&agent.name);
                    for group in &agent.groups {
                        self.group(group);
                    }
                }
            }
            Data::Groups(Ok(list)) => {
                for group in &list.groups {
                    self.group(&group.name);
                }
            }
            Data::Feed {
                result: Ok(rows), ..
            } => {
                for row in rows {
                    if row.control.is_some() || row.id <= 0 {
                        continue;
                    }
                    if row.from_kind() == proto::Kind::Agent || row.from_owner.is_some() {
                        self.agent(&row.from_name);
                    }
                    if row.to_owner.is_some()
                        && let Some(to) = &row.to_name
                    {
                        self.agent(to);
                    }
                    if let Some(group) = &row.to_group {
                        self.group(group);
                    }
                    if row.body.len() > MAX_TEXT_BYTES {
                        continue;
                    }
                    if self.messages.get(&row.id) == Some(row) {
                        continue;
                    }
                    if let Some(old) = self.messages.insert(row.id, row.clone()) {
                        self.text_bytes -= old.body.len();
                    }
                    self.text_bytes += row.body.len();
                    self.revision = self.revision.wrapping_add(1);
                }
                self.prune(MAX_MESSAGES, MAX_TEXT_BYTES);
            }
            _ => {}
        }
    }

    /// The app calls this only for its current summary subscription.
    pub fn summaries(&mut self, rows: &[proto::AgentSummary]) {
        for row in rows {
            self.agent(&row.agent);
        }
    }

    fn agent(&mut self, name: &str) {
        let key = name.to_lowercase();
        if !name.is_empty() && !self.retired.contains(&key) {
            if self.agents.insert(key, name.into()).as_deref() != Some(name) {
                self.revision = self.revision.wrapping_add(1);
            }
        }
    }

    fn group(&mut self, name: &str) {
        if !name.is_empty() {
            if self
                .groups
                .insert(name.to_lowercase(), name.into())
                .as_deref()
                != Some(name)
            {
                self.revision = self.revision.wrapping_add(1);
            }
        }
    }

    fn prune(&mut self, count: usize, bytes: usize) {
        while self.messages.len() > count || self.text_bytes > bytes {
            let Some((_, old)) = self.messages.pop_first() else {
                break;
            };
            self.text_bytes -= old.body.len();
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn message(&self, id: i64) -> Option<&proto::Message> {
        self.messages.get(&id)
    }

    pub fn search(&self, query: &str) -> Vec<Hit> {
        let query: String = query.trim().chars().map(fold).collect();
        let mut hits = Vec::new();
        for (names, agent) in [(&self.agents, true), (&self.groups, false)] {
            for name in names.values() {
                if let Some(rank) = rank(&query, name) {
                    hits.push(Hit {
                        target: if agent {
                            Target::Agent(name.clone())
                        } else {
                            Target::Group(name.clone())
                        },
                        text: name.clone(),
                        rank,
                    });
                }
            }
        }
        for row in self.messages.values().rev() {
            let best = row
                .body
                .lines()
                .filter_map(|line| rank(&query, line).map(|rank| (rank, line)))
                .min_by_key(|(rank, _)| *rank);
            if let Some((rank, line)) = best {
                hits.push(Hit {
                    target: Target::Message(row.id),
                    text: snippet(line, rank.start),
                    rank,
                });
            }
        }
        hits.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then_with(|| a.target.category().cmp(&b.target.category()))
                .then_with(|| b.target.message_id().cmp(&a.target.message_id()))
                .then_with(|| a.text.to_lowercase().cmp(&b.text.to_lowercase()))
                .then_with(|| a.text.cmp(&b.text))
        });
        hits.truncate(MAX_RESULTS);
        hits
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Cancel,
    Choose(Target),
    Quit,
}

/// A single-line query and local result selection. It never owns a connection.
pub struct Picker {
    query: TextArea<'static>,
    hits: Vec<Hit>,
    selected: usize,
    revision: u64,
}

impl Picker {
    pub fn new(cache: &Cache) -> Picker {
        let mut query = TextArea::default();
        query.set_placeholder_text("Search agents, groups and messages");
        Picker {
            query,
            hits: cache.search(""),
            selected: 0,
            revision: cache.revision,
        }
    }

    pub fn refresh(&mut self, cache: &Cache) {
        if self.revision == cache.revision {
            return;
        }
        let selected = self.hits.get(self.selected).map(|h| h.target.clone());
        self.hits = cache.search(&self.query.lines()[0]);
        self.selected = selected
            .and_then(|target| self.hits.iter().position(|h| h.target == target))
            .unwrap_or(0);
        self.revision = cache.revision;
    }

    fn edited(&mut self, cache: &Cache) {
        self.hits = cache.search(&self.query.lines()[0]);
        self.selected = 0;
        self.revision = cache.revision;
    }

    pub fn paste(&mut self, text: &str, cache: &Cache) {
        let room = MAX_QUERY_CHARS.saturating_sub(self.query.lines()[0].chars().count());
        let text: String = text
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .filter(|c| !c.is_control())
            .take(room)
            .collect();
        self.query.insert_str(text);
        self.edited(cache);
    }

    pub fn key(&mut self, key: KeyEvent, cache: &Cache) -> Outcome {
        self.refresh(cache);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => return Outcome::Quit,
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(self.hits.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                return self
                    .hits
                    .get(self.selected)
                    .map_or(Outcome::Stay, |h| Outcome::Choose(h.target.clone()));
            }
            // Enter's control aliases must not create a second query line.
            KeyCode::Char('m' | 'j') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                self.selected = (self.selected + 1).min(self.hits.len().saturating_sub(1))
            }
            KeyCode::Char('\n' | '\r') => {}
            _ => {
                let before = self.query.lines()[0].clone();
                if matches!(key.code, KeyCode::Char(_))
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && before.chars().count() >= MAX_QUERY_CHARS
                {
                    return Outcome::Stay;
                }
                self.query.input(key);
                if self.query.lines()[0] != before {
                    self.edited(cache);
                }
            }
        }
        Outcome::Stay
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, cache: &Cache) {
        self.refresh(cache);
        frame.render_widget(Clear, area);
        frame.render_widget(Block::new().style(theme::base()), area);
        if theme::too_small(area) {
            return;
        }
        let shown = usize::from(area.height.saturating_sub(9) / 2).clamp(1, 8);
        let width = area.width.saturating_sub(4).min(88);
        let height = 7 + shown as u16 * 2;
        let panel = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 3,
            width,
            height,
        );
        let surface = theme::style(Role::HumanSurface);
        frame.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .title(" Go to · cached data ")
                .style(theme::on_surface(surface, theme::style(Role::Accent))),
            panel,
        );
        let inner = panel.inner(Margin::new(2, 1));
        self.query.set_style(surface);
        self.query.set_cursor_line_style(surface);
        self.query.set_cursor_style(theme::style(Role::Cursor));
        self.query
            .set_selection_style(theme::style(Role::Selection));
        self.query
            .set_placeholder_style(theme::on_surface(surface, theme::style(Role::Muted)));
        frame.render_widget(&self.query, Rect::new(inner.x, inner.y, inner.width, 1));
        let first = self.selected.saturating_sub(shown - 1);
        if self.hits.is_empty() {
            frame.render_widget(
                Paragraph::new("No cached matches")
                    .style(theme::on_surface(surface, theme::style(Role::Muted))),
                Rect::new(inner.x, inner.y + 2, inner.width, 1),
            );
        }
        for (at, hit) in self.hits.iter().enumerate().skip(first).take(shown) {
            let y = inner.y + 2 + (at - first) as u16 * 2;
            let row_style = if at == self.selected {
                theme::style(Role::Selection)
            } else {
                surface
            };
            let (title, detail, role) = match &hit.target {
                Target::Agent(name) => (
                    format!("Agent · {name}"),
                    "Agent conversation".to_string(),
                    Role::Agent,
                ),
                Target::Group(name) => (
                    format!("Group · @{name}"),
                    "Group conversation".to_string(),
                    Role::Group,
                ),
                Target::Message(id) => {
                    let participants = cache
                        .message(*id)
                        .map(|m| {
                            let to = m
                                .to_group
                                .as_ref()
                                .map(|g| format!("@{g}"))
                                .or_else(|| m.to_name.clone())
                                .unwrap_or_default();
                            format!(" · {} → {to}", m.from_name)
                        })
                        .unwrap_or_default();
                    (
                        format!("Message #{id}{participants}"),
                        hit.text.clone(),
                        Role::Text,
                    )
                }
            };
            let lines = vec![
                theme::clip_line(
                    Line::from(Span::styled(
                        title,
                        theme::on_surface(row_style, theme::style(role)),
                    )),
                    inner.width as usize,
                ),
                theme::clip_line(Line::raw(detail), inner.width as usize),
            ];
            frame.render_widget(
                Paragraph::new(lines).style(row_style),
                Rect::new(inner.x, y, inner.width, 2),
            );
        }
        let footer = format!(
            "↑/↓ select · Enter opens · Esc back · {} results",
            self.hits.len()
        );
        frame.render_widget(
            Paragraph::new(theme::clip_line(Line::raw(footer), inner.width as usize))
                .style(theme::on_surface(surface, theme::style(Role::Muted))),
            Rect::new(inner.x, panel.bottom() - 2, inner.width, 1),
        );
    }
}

/// Exact, prefix, word-boundary substring, substring, then ordered letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    tier: u8,
    gaps: usize,
    start: usize,
    length: usize,
}

fn rank(query: &str, text: &str) -> Option<Rank> {
    let folded: String = text.chars().map(fold).collect();
    let length = folded.chars().count();
    let base = Rank {
        tier: 0,
        gaps: 0,
        start: 0,
        length,
    };
    if query.is_empty() {
        return Some(Rank { length: 0, ..base });
    }
    if folded == query {
        return Some(base);
    }
    if folded.starts_with(query) {
        return Some(Rank { tier: 1, ..base });
    }
    if let Some(at) = folded.find(query) {
        let prefix = &folded[..at];
        let boundary = prefix
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric());
        return Some(Rank {
            tier: if boundary { 2 } else { 3 },
            start: prefix.chars().count(),
            ..base
        });
    }
    let mut letters = query.chars();
    let mut next = letters.next()?;
    let mut start = None;
    for (i, c) in folded.chars().enumerate() {
        if c != next {
            continue;
        }
        let first = *start.get_or_insert(i);
        match letters.next() {
            Some(c) => next = c,
            None => {
                return Some(Rank {
                    tier: 4,
                    gaps: i + 1 - first - query.chars().count(),
                    start: first,
                    length,
                });
            }
        }
    }
    None
}

fn snippet(text: &str, at: usize) -> String {
    let start = at.saturating_sub(24);
    let mut shown = String::new();
    if start > 0 {
        shown.push('…');
    }
    shown.extend(
        text.chars()
            .skip(start)
            .take(100)
            .map(|c| if c.is_control() { ' ' } else { c }),
    );
    if text.chars().count() > start + 100 {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn feed(id: i64, body: &str) -> Data {
        Data::Feed {
            id: 7,
            result: Ok(vec![proto::Message {
                id,
                body: body.into(),
                ..Default::default()
            }]),
        }
    }

    #[test]
    fn ranking_prefers_exact_then_prefix_then_words_and_keeps_unicode() {
        let mut cache = Cache::default();
        for name in ["Mateo", "Mat", "Format", "M a t", "Foo Mat"] {
            cache.agent(name);
        }
        let names: Vec<_> = cache.search("MAT").into_iter().map(|h| h.text).collect();
        assert_eq!(names, ["Mat", "Mateo", "Foo Mat", "Format", "M a t"]);
        cache.agent("Équipe");
        assert_eq!(cache.search("éq")[0].text, "Équipe");
        assert!(cache.search("eq").is_empty(), "accents remain significant");
        assert!(rank("thoe", "Theo").is_none(), "letters stay in order");
    }

    #[test]
    fn received_messages_deduplicate_keep_the_best_line_and_evict_within_bounds() {
        let mut cache = Cache::default();
        cache.observe(&feed(1, "irrelevant\nneedle\nmore text"));
        cache.observe(&feed(2, "needle in a longer line"));
        let hits = cache.search("needle");
        assert_eq!(hits[0].target, Target::Message(1));
        assert_eq!(hits[0].text, "needle");
        cache.observe(&feed(1, "updated body"));
        assert_eq!(cache.messages.len(), 2);
        assert_eq!(
            cache.text_bytes,
            "updated body".len() + "needle in a longer line".len()
        );
        cache.prune(1, usize::MAX);
        assert!(cache.message(1).is_none());
        assert!(cache.message(2).is_some());
        cache.prune(1, 1);
        assert_eq!(cache.text_bytes, 0);
        assert!(cache.messages.is_empty());
    }

    fn key(picker: &mut Picker, cache: &Cache, code: KeyCode) -> Outcome {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE), cache)
    }

    #[test]
    fn enter_selects_the_best_local_hit_and_arrows_select_other_hits() {
        let mut cache = Cache::default();
        cache.agent("Mat");
        cache.agent("Mateo");
        cache.group("matters");
        cache.observe(&feed(8, "format"));
        let mut picker = Picker::new(&cache);
        picker.paste("mat", &cache);
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Agent("Mat".into()))
        );
        key(&mut picker, &cache, KeyCode::Down);
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Agent("Mateo".into()))
        );
        key(&mut picker, &cache, KeyCode::Tab);
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Group("matters".into()))
        );
        for _ in 0..100 {
            key(&mut picker, &cache, KeyCode::Down);
        }
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Message(8))
        );
        picker.paste("zzzz", &cache);
        assert_eq!(key(&mut picker, &cache, KeyCode::Enter), Outcome::Stay);
        assert_eq!(key(&mut picker, &cache, KeyCode::Esc), Outcome::Cancel);
        assert_eq!(
            picker.key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &cache
            ),
            Outcome::Quit
        );
    }

    #[test]
    fn the_query_is_single_line_utf8_editable_and_bounded() {
        let mut cache = Cache::default();
        cache.agent("Équipe");
        let mut picker = Picker::new(&cache);
        picker.paste("é\r\nq\t\u{1b}", &cache);
        assert_eq!(picker.query.lines(), ["é q "]);
        for code in ['m', 'j'] {
            picker.key(
                KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL),
                &cache,
            );
        }
        assert_eq!(picker.query.lines(), ["é q "]);
        key(&mut picker, &cache, KeyCode::Home);
        key(&mut picker, &cache, KeyCode::Delete);
        assert_eq!(picker.query.lines(), [" q "]);
        key(&mut picker, &cache, KeyCode::End);
        picker.paste(&"界".repeat(MAX_QUERY_CHARS), &cache);
        assert_eq!(picker.query.lines().len(), 1);
        assert_eq!(picker.query.lines()[0].chars().count(), MAX_QUERY_CHARS);
        key(&mut picker, &cache, KeyCode::Char('x'));
        assert_eq!(picker.query.lines()[0].chars().count(), MAX_QUERY_CHARS);
        key(&mut picker, &cache, KeyCode::Backspace);
        key(&mut picker, &cache, KeyCode::Char('é'));
        assert!(picker.query.lines()[0].ends_with('é'));
    }

    #[test]
    fn passive_updates_preserve_the_selected_identity_until_it_disappears() {
        let mut cache = Cache::default();
        cache.agent("Beta");
        cache.agent("Gamma");
        let mut picker = Picker::new(&cache);
        key(&mut picker, &cache, KeyCode::Down);
        cache.agent("Alpha");
        picker.refresh(&cache);
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Agent("Gamma".into()))
        );
        let revision = cache.revision;
        cache.summaries(&[proto::AgentSummary {
            agent: "Gamma".into(),
            ..Default::default()
        }]);
        assert_eq!(
            cache.revision, revision,
            "unchanged summaries do not rerank"
        );
        cache.observe(&Data::Agents(Ok(proto::ListAgentsResponse::default())));
        picker.refresh(&cache);
        assert!(picker.hits.is_empty());
        assert_eq!(key(&mut picker, &cache, KeyCode::Enter), Outcome::Stay);
    }

    #[test]
    fn the_picker_covers_the_screen_and_keeps_input_and_selection_visible() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for width in [60, 80] {
                    let mut cache = Cache::default();
                    for i in 0..30 {
                        cache.agent(&format!("Agent{i:02}"));
                    }
                    let mut picker = Picker::new(&cache);
                    picker.paste("Agent", &cache);
                    for _ in 0..25 {
                        key(&mut picker, &cache, KeyCode::Down);
                    }
                    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(
                                Paragraph::new(vec![Line::raw("Z".repeat(width as usize)); 24]),
                                f.area(),
                            );
                            picker.render(f, f.area(), &cache);
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    assert_buffer(buffer);
                    let lines: Vec<String> = (0..24)
                        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                        .collect();
                    let text = lines.join("\n");
                    assert!(
                        !text.contains('Z'),
                        "the origin must not show through: {text}"
                    );
                    assert!(text.contains("Go to · cached data"), "{text}");
                    assert!(
                        text.contains("Agent · Agent25"),
                        "the selected result stays in view: {text}"
                    );
                    assert!(text.contains("Enter opens"), "{text}");
                    let query = lines
                        .iter()
                        .find(|line| line.trim_matches([' ', '│']) == "Agent")
                        .expect("query has its own row");
                    assert!(
                        query.find("Agent").unwrap() > 2,
                        "input is inset into the centered panel"
                    );
                }
            });
        }
    }
}
