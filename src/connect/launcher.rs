//! Local launcher data. Nothing here opens a backend or creates requests.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui_textarea::TextArea;

use super::event::Data;
use super::navigation::Conversation;
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

#[derive(Clone, Debug)]
pub struct Hit {
    pub target: Target,
    pub text: String,
}

/// Previously received data, bounded for this client session only.
#[derive(Default)]
pub struct Cache {
    agents: BTreeMap<String, String>,
    groups: BTreeMap<String, String>,
    messages: BTreeMap<i64, CachedMessage>,
    text_bytes: usize,
    revision: u64,
}

struct CachedMessage {
    message: proto::Message,
    /// Conversations that actually delivered this cached row to the app.
    sources: Vec<Conversation>,
    /// Indexed once when body text arrives, never on a picker keystroke.
    folded: String,
}

fn folded(text: &str) -> String {
    text.chars().map(fold).collect()
}

impl Cache {
    pub fn observe(&mut self, data: &Data) {
        self.observe_from(data, None);
    }

    pub fn observe_from(&mut self, data: &Data, source: Option<&Conversation>) {
        match data {
            Data::Agents(Ok(list)) => {
                self.revision = self.revision.wrapping_add(1);
                self.agents.clear();
                for agent in list.agents.iter().filter(|a| !a.retired) {
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
                    if let Some(old) = self.messages.get_mut(&row.id) {
                        if let Some(source) = source
                            && !old.sources.iter().any(|s| s.same(source))
                        {
                            old.sources.push(source.clone());
                        }
                        if old.message == *row {
                            continue;
                        }
                        if old.message.body == row.body
                            && old.message.from_name == row.from_name
                            && old.message.to_name == row.to_name
                            && old.message.to_group == row.to_group
                        {
                            // Read receipts affect navigation, but not picker results.
                            old.message = row.clone();
                            continue;
                        }
                    }
                    let (indexed, sources) = match self.messages.remove(&row.id) {
                        Some(old) => {
                            self.text_bytes -= old.message.body.len();
                            let indexed = if old.message.body == row.body {
                                old.folded
                            } else {
                                folded(&row.body)
                            };
                            (indexed, old.sources)
                        }
                        None => (folded(&row.body), source.into_iter().cloned().collect()),
                    };
                    self.messages.insert(
                        row.id,
                        CachedMessage {
                            message: row.clone(),
                            folded: indexed,
                            sources,
                        },
                    );
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
        if !name.is_empty()
            && self.agents.insert(folded(name), name.into()).as_deref() != Some(name)
        {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    fn group(&mut self, name: &str) {
        if !name.is_empty()
            && self.groups.insert(folded(name), name.into()).as_deref() != Some(name)
        {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    fn prune(&mut self, count: usize, bytes: usize) {
        while self.messages.len() > count || self.text_bytes > bytes {
            let Some((_, old)) = self.messages.pop_first() else {
                break;
            };
            self.text_bytes -= old.message.body.len();
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn message(&self, id: i64) -> Option<&proto::Message> {
        self.messages.get(&id).map(|cached| &cached.message)
    }

    /// A broadcast opens its group; direct mail opens its agent's conversation.
    /// Mail without an agent participant belongs in Feed.
    pub fn destination(&self, id: i64) -> Option<Conversation> {
        let row = self.message(id)?;
        if let Some(group) = &row.to_group {
            Some(Conversation::Group(group.clone()))
        } else if row.from_kind() == proto::Kind::Agent {
            Some(Conversation::Agent(row.from_name.clone()))
        } else if row.to_owner.is_some() {
            row.to_name
                .as_ref()
                .map(|name| Conversation::Agent(name.clone()))
        } else {
            None
        }
    }

    /// Only already received rows. A selected hit gets a bounded window around
    /// it, rather than the latest window, without fetching older history.
    pub fn context(
        &self,
        destination: Option<&Conversation>,
        hit: Option<i64>,
    ) -> Vec<proto::Message> {
        let rows: Vec<_> = self
            .messages
            .values()
            .filter(|cached| {
                let m = &cached.message;
                match destination {
                    None => true,
                    Some(Conversation::Group(group)) => m
                        .to_group
                        .as_ref()
                        .is_some_and(|g| g.eq_ignore_ascii_case(group)),
                    Some(scope @ Conversation::Agent(agent)) => {
                        cached.sources.iter().any(|s| s.same(scope))
                            || (m.from_kind() == proto::Kind::Agent
                                && m.from_name.eq_ignore_ascii_case(agent))
                            || (m.to_owner.is_some()
                                && m.to_name
                                    .as_ref()
                                    .is_some_and(|to| to.eq_ignore_ascii_case(agent)))
                    }
                }
            })
            .collect();
        let rows = if let Some(at) = hit.and_then(|id| rows.iter().position(|r| r.message.id == id))
        {
            let start = at.saturating_sub(100).min(rows.len().saturating_sub(200));
            &rows[start..rows.len().min(start + 200)]
        } else {
            &rows[..]
        };
        rows.iter().map(|cached| cached.message.clone()).collect()
    }

    pub fn search(&self, query: &str) -> Vec<Hit> {
        let query = folded(query.trim());
        // The open state is names, then recent mail: no body scan or ranking.
        if query.is_empty() {
            return self
                .agents
                .values()
                .map(|name| Hit {
                    target: Target::Agent(name.clone()),
                    text: name.clone(),
                })
                .chain(self.groups.values().map(|name| Hit {
                    target: Target::Group(name.clone()),
                    text: name.clone(),
                }))
                .chain(self.messages.values().rev().map(|cached| Hit {
                    target: Target::Message(cached.message.id),
                    text: snippet(cached.message.body.lines().next().unwrap_or_default(), 0),
                }))
                .take(MAX_RESULTS)
                .collect();
        }
        // Candidates borrow cached text. Only the returned hits allocate labels
        // or snippets; ranking takes an already folded slice and allocates none.
        let mut candidates = Vec::new();
        for (names, agent) in [(&self.agents, true), (&self.groups, false)] {
            for (indexed, name) in names {
                if let Some(rank) = rank(&query, indexed) {
                    candidates.push(Candidate {
                        kind: if agent { 0 } else { 1 },
                        id: 0,
                        text: name,
                        indexed,
                        rank,
                    });
                }
            }
        }
        for cached in self.messages.values().rev() {
            let best = cached
                .message
                .body
                .lines()
                .zip(cached.folded.lines())
                .filter_map(|(line, indexed)| {
                    rank(&query, indexed).map(|rank| (rank, line, indexed))
                })
                .min_by_key(|(rank, _, _)| *rank);
            if let Some((rank, text, indexed)) = best {
                candidates.push(Candidate {
                    kind: 2,
                    id: cached.message.id,
                    text,
                    indexed,
                    rank,
                });
            }
        }
        if candidates.len() > MAX_RESULTS {
            candidates.select_nth_unstable_by(MAX_RESULTS, Candidate::compare);
            candidates.truncate(MAX_RESULTS);
        }
        candidates.sort_by(Candidate::compare);
        candidates
            .into_iter()
            .map(|hit| Hit {
                target: match hit.kind {
                    0 => Target::Agent(hit.text.into()),
                    1 => Target::Group(hit.text.into()),
                    _ => Target::Message(hit.id),
                },
                text: if hit.kind == 2 {
                    snippet(hit.text, hit.rank.start)
                } else {
                    hit.text.into()
                },
            })
            .collect()
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
            // Ctrl-M must not create a second query line.
            KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char('p' | 'k') if key.modifiers == KeyModifiers::CONTROL => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Char('n' | 'j') if key.modifiers == KeyModifiers::CONTROL => {
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
            "↑/↓ Ctrl-K/J · Enter opens · Esc back · {}{} results",
            if self.hits.len() == MAX_RESULTS {
                "top "
            } else {
                ""
            },
            self.hits.len()
        );
        frame.render_widget(
            Paragraph::new(theme::clip_line(Line::raw(footer), inner.width as usize))
                .style(theme::on_surface(surface, theme::style(Role::Muted))),
            Rect::new(inner.x, panel.bottom() - 2, inner.width, 1),
        );
    }
}

/// Borrowed candidates keep search work independent of snippet allocations.
struct Candidate<'a> {
    kind: u8,
    id: i64,
    text: &'a str,
    indexed: &'a str,
    rank: Rank,
}

impl Candidate<'_> {
    fn compare(a: &Self, b: &Self) -> std::cmp::Ordering {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.rank.tier.cmp(&b.rank.tier))
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| a.rank.cmp(&b.rank))
            .then_with(|| a.indexed.cmp(b.indexed))
            .then_with(|| a.text.cmp(b.text))
    }
}

/// Exact, two/one/no word boundaries, then ordered letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    tier: u8,
    gaps: usize,
    start: usize,
    length: usize,
}

fn rank(query: &str, folded: &str) -> Option<Rank> {
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
    if let Some((tier, at)) = folded
        .match_indices(query)
        .map(|(at, _)| {
            let prefix = &folded[..at];
            let starts_word = prefix
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            let ends_word = folded[at + query.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            (3 - u8::from(starts_word) - u8::from(ends_word), at)
        })
        .min()
    {
        return Some(Rank {
            tier,
            start: folded[..at].chars().count(),
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
    fn ranking_prefers_exact_then_words_then_partial_and_keeps_unicode() {
        let mut cache = Cache::default();
        for name in ["Mateo", "Mat", "Format", "M a t", "Foo Mat"] {
            cache.agent(name);
        }
        let names: Vec<_> = cache.search("MAT").into_iter().map(|h| h.text).collect();
        assert_eq!(names, ["Mat", "Foo Mat", "Mateo", "Format", "M a t"]);
        cache.agent("Équipe");
        assert_eq!(cache.search("éq")[0].text, "Équipe");
        assert!(cache.search("eq").is_empty(), "accents remain significant");
        assert!(rank("thoe", "theo").is_none(), "letters stay in order");
    }

    #[test]
    fn ranking_groups_names_then_prefers_whole_words_and_newer_messages() {
        let mut cache = Cache::default();
        cache.agent("Jefe");
        cache.agent("Jeferson");
        cache.group("Jefe crew");
        cache.observe(&feed(1, "Hello Jefe, how are you?"));
        cache.observe(&feed(2, "blah blah abcJefedefgh"));
        cache.observe(&feed(3, "Jeferson"));
        cache.observe(&feed(4, "a much longer message: Hello Jefe, how are you?"));
        cache.observe(&feed(5, "abcJefedefgh, then Jefe!"));
        cache.observe(&feed(6, "abcJefe"));
        let hits = cache.search("Jefe");
        assert_eq!(
            hits.iter().map(|h| h.target.clone()).collect::<Vec<_>>(),
            [
                Target::Agent("Jefe".into()),
                Target::Agent("Jeferson".into()),
                Target::Group("Jefe crew".into()),
                Target::Message(5),
                Target::Message(4),
                Target::Message(1),
                Target::Message(6),
                Target::Message(3),
                Target::Message(2),
            ]
        );
        assert_eq!(rank("jefe", "abcjefedefgh, then jefe!").unwrap().start, 19);
        assert!(rank("jefe", "hello jefe!") < rank("jefe", "jeferson"));
        assert!(rank("jefe", "hello jefe!") < rank("jefe", "hello jeferson"));
        assert!(rank("jefe", "hello jefe!") < rank("jefe", "界jefe界"));
        assert_eq!(rank("jefe", "«jefe»").unwrap().tier, 1);
        assert_eq!(rank("jefe", "abcjefe").unwrap().tier, 2);
        assert_eq!(rank("jefe", "jeferson").unwrap().tier, 2);
        assert_eq!(rank("jefe", "abcjefedef").unwrap().tier, 3);
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

    #[test]
    fn indexing_keeps_original_unicode_lines_and_receipts_do_not_rerank() {
        let mut cache = Cache::default();
        let mut data = feed(12, "irrelevant\r\nİΣ ÉQUIPE\r\nlast");
        cache.observe(&data);
        assert_eq!(cache.search("İσ éq")[0].text, "İΣ ÉQUIPE");
        let revision = cache.revision;
        let index = cache.messages[&12].folded.as_ptr();
        if let Data::Feed {
            result: Ok(rows), ..
        } = &mut data
        {
            rows[0].read_at = Some("2026-10-22T19:00:00Z".into());
        }
        cache.observe(&data);
        assert_eq!(cache.revision, revision);
        assert_eq!(cache.messages[&12].folded.as_ptr(), index);
        assert!(cache.message(12).unwrap().read_at.is_some());
        cache.observe(&feed(12, "a different line\n界 ÉQUIPE"));
        assert!(cache.search("İσ").is_empty());
        assert_eq!(cache.search("界 éq")[0].text, "界 ÉQUIPE");
        assert!(cache.revision > revision);
    }

    #[test]
    fn cached_context_keeps_actual_group_deliveries_and_routes_each_message_kind() {
        let mut cache = Cache::default();
        let mateo = Conversation::Agent("Mateo".into());
        let data = Data::Feed {
            id: 1,
            result: Ok(vec![proto::Message {
                id: 1,
                from_kind: proto::Kind::Agent as i32,
                from_name: "Zed".into(),
                to_group: Some("crew".into()),
                body: "a delivered broadcast".into(),
                ..Default::default()
            }]),
        };
        cache.observe_from(&data, Some(&mateo));
        cache.observe_from(&data, Some(&mateo));
        assert_eq!(cache.messages[&1].sources.len(), 1);
        assert_eq!(cache.context(Some(&mateo), None)[0].id, 1);
        assert_eq!(
            cache.destination(1),
            Some(Conversation::Group("crew".into()))
        );
        for (id, from_kind, from_name, to_name, to_owner) in [
            (2, proto::Kind::Agent, "Zed", "ada", None),
            (3, proto::Kind::Human, "ada", "Mateo", Some("ada")),
            (4, proto::Kind::Human, "ada", "bob", None),
        ] {
            cache.observe(&Data::Feed {
                id: 2,
                result: Ok(vec![proto::Message {
                    id,
                    from_kind: from_kind as i32,
                    from_name: from_name.into(),
                    to_name: Some(to_name.into()),
                    to_owner: to_owner.map(Into::into),
                    body: "cached".into(),
                    ..Default::default()
                }]),
            });
        }
        assert_eq!(
            cache.destination(2),
            Some(Conversation::Agent("Zed".into()))
        );
        assert_eq!(cache.destination(3), Some(mateo.clone()));
        assert_eq!(cache.destination(4), None, "human mail belongs in Feed");
        assert_eq!(
            cache
                .context(Some(&mateo), None)
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(cache.context(None, None).len(), 4);
    }

    #[test]
    fn a_message_selection_seeds_its_cached_neighbors_instead_of_only_the_latest_window() {
        let mut cache = Cache::default();
        cache.observe(&Data::Feed {
            id: 1,
            result: Ok((1..=1000)
                .map(|id| proto::Message {
                    id,
                    from_kind: proto::Kind::Agent as i32,
                    from_name: "Zed".into(),
                    body: format!("message {id}"),
                    ..Default::default()
                })
                .collect()),
        });
        let zed = Conversation::Agent("zed".into());
        for id in [1, 500, 1000] {
            let rows = cache.context(Some(&zed), Some(id));
            assert_eq!(rows.len(), 200);
            assert!(rows.iter().any(|m| m.id == id));
            assert!(rows.windows(2).all(|w| w[0].id < w[1].id));
        }
        assert_eq!(cache.context(Some(&zed), Some(1))[0].id, 1);
        assert_eq!(cache.context(Some(&zed), Some(500))[100].id, 500);
    }

    #[test]
    fn searching_the_full_cache_indexes_once_and_materializes_only_top_hits() {
        use std::time::Instant;
        let mut cache = Cache::default();
        // 10,000 × 1,600 bytes: close to the text budget with every slot used.
        let body = "A long line of cached message text.\n".repeat(45);
        let body = format!("{}\nneedle", &body[..1593]);
        assert_eq!(body.len(), 1600);
        let rows = (1..=MAX_MESSAGES as i64)
            .map(|id| proto::Message {
                id,
                body: body.clone(),
                ..Default::default()
            })
            .collect();
        cache.observe(&Data::Feed {
            id: 1,
            result: Ok(rows),
        });
        assert_eq!(cache.messages.len(), MAX_MESSAGES);
        assert_eq!(cache.text_bytes, 16_000_000);
        let index = cache.messages[&1].folded.as_ptr();
        let opened = Instant::now();
        let mut picker = Picker::new(&cache);
        let opening = opened.elapsed();
        assert_eq!(picker.hits.len(), MAX_RESULTS);
        assert_eq!(picker.hits[0].target, Target::Message(MAX_MESSAGES as i64));
        picker.paste("needl", &cache);
        let started = Instant::now();
        key(&mut picker, &cache, KeyCode::Char('e'));
        let typing = started.elapsed();
        assert_eq!(picker.hits.len(), MAX_RESULTS);
        assert!(picker.hits.iter().all(|hit| hit.text == "needle"));
        assert_eq!(picker.hits[0].target, Target::Message(MAX_MESSAGES as i64));
        assert_eq!(
            picker.hits[MAX_RESULTS - 1].target,
            Target::Message((MAX_MESSAGES - MAX_RESULTS + 1) as i64)
        );
        assert_eq!(cache.messages[&1].folded.as_ptr(), index);
        let started = Instant::now();
        assert!(cache.search("zzzz").is_empty());
        eprintln!(
            "full cache: open={opening:?}, one key={typing:?}, no match={:?}",
            started.elapsed()
        );
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
        picker.key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &cache,
        );
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Agent("Mateo".into()))
        );
        picker.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &cache,
        );
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
        picker.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &cache,
        );
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Message(8))
        );
        for _ in 0..100 {
            picker.key(
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
                &cache,
            );
        }
        assert_eq!(
            key(&mut picker, &cache, KeyCode::Enter),
            Outcome::Choose(Target::Agent("Mat".into()))
        );
        assert_eq!(picker.query.lines(), ["mat"], "navigation keeps the query");
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
