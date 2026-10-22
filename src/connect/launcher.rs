//! Local launcher data. Nothing here opens a backend or creates requests.

use std::collections::{BTreeMap, HashSet};

use super::event::Data;
use crate::proto;
use crate::ui::message::fold;

const MAX_MESSAGES: usize = 10_000;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 50;

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
}

impl Cache {
    pub fn observe(&mut self, data: &Data) {
        match data {
            Data::Agents(Ok(list)) => {
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
                    if let Some(old) = self.messages.insert(row.id, row.clone()) {
                        self.text_bytes -= old.body.len();
                    }
                    self.text_bytes += row.body.len();
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
            self.agents.insert(key, name.into());
        }
    }

    fn group(&mut self, name: &str) {
        if !name.is_empty() {
            self.groups.insert(name.to_lowercase(), name.into());
        }
    }

    fn prune(&mut self, count: usize, bytes: usize) {
        while self.messages.len() > count || self.text_bytes > bytes {
            let Some((_, old)) = self.messages.pop_first() else {
                break;
            };
            self.text_bytes -= old.body.len();
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
}
