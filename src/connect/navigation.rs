//! Session conversation history and the one shared unread-mail subscription.

use super::app::next_id;
use super::event::Request;
use crate::proto;
use chrono::{DateTime, FixedOffset};
use std::collections::{HashMap, HashSet};

pub enum Feedback {
    Summary(String),
    Other(String),
    ReadError { agent: String, text: String },
}

impl Feedback {
    pub fn text(&self) -> &str {
        match self {
            Self::Summary(text) | Self::Other(text) | Self::ReadError { text, .. } => text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conversation {
    Agent(String),
    Group(String),
}

impl Conversation {
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Agent(a), Self::Agent(b)) | (Self::Group(a), Self::Group(b)) => {
                a.eq_ignore_ascii_case(b)
            }
            _ => false,
        }
    }
}

#[derive(Default)]
pub struct History {
    entries: Vec<Conversation>,
    cursor: Option<usize>,
}

impl History {
    pub fn visit(&mut self, target: Conversation) {
        self.entries.retain(|entry| !entry.same(&target));
        self.entries.push(target);
        self.cursor = Some(self.entries.len() - 1);
    }

    pub fn back(&mut self, available: impl Fn(&Conversation) -> bool) -> Option<Conversation> {
        // Discard only targets known unavailable from successful server lists.
        let current = self.cursor.and_then(|i| self.entries.get(i)).cloned();
        self.entries.retain(&available);
        if self.entries.is_empty() {
            self.cursor = None;
            return None;
        }
        let at = current.and_then(|c| self.entries.iter().position(|e| e.same(&c)));
        let at = at.map_or(self.entries.len() - 1, |i| {
            (i + self.entries.len() - 1) % self.entries.len()
        });
        self.cursor = Some(at);
        Some(self.entries[at].clone())
    }
}

#[derive(Clone, Copy)]
enum Read {
    InFlight,
    AwaitRefresh,
    Baseline(u64),
}

/// Shared by lists, keyboard navigation and the conversation mailbox.
pub struct Inbox {
    pub id: u64,
    pub summaries: HashMap<String, proto::AgentSummary>,
    pub error: Option<String>,
    pub loaded: bool,
    pending: HashMap<String, Read>,
    attempted: HashMap<String, i64>,
    failed: HashSet<String>,
    refresh: bool,
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            id: next_id(),
            summaries: HashMap::new(),
            error: None,
            loaded: false,
            pending: HashMap::new(),
            attempted: HashMap::new(),
            failed: HashSet::new(),
            refresh: false,
        }
    }
}

impl Inbox {
    pub fn snapshot(&mut self, id: u64, result: &Result<Vec<proto::AgentSummary>, String>) {
        if id != self.id {
            return;
        }
        match result {
            Ok(rows) => {
                self.summaries = rows
                    .iter()
                    .map(|s| (s.agent.to_ascii_lowercase(), s.clone()))
                    .collect();
                self.error = None;
                self.loaded = true;
                self.pending.retain(
                    |_, read| !matches!(read, Read::Baseline(generation) if *generation == id),
                );
            }
            Err(error) => {
                self.error = Some(error.clone());
                self.summaries.clear();
                self.loaded = false;
                self.pending
                    .retain(|_, read| matches!(read, Read::InFlight));
            }
        }
    }

    pub fn oldest(&self) -> Option<String> {
        if !self.loaded {
            return None;
        }
        self.summaries
            .values()
            .filter(|s| {
                s.unread_messages > 0 && !self.pending.contains_key(&s.agent.to_ascii_lowercase())
            })
            .filter_map(|s| {
                Some((
                    DateTime::parse_from_rfc3339(s.oldest_unread_message.as_deref()?).ok()?,
                    s.agent.to_ascii_lowercase(),
                    s.agent.clone(),
                ))
            })
            .min_by(|a: &(DateTime<FixedOffset>, String, String), b| {
                (&a.0, &a.1).cmp(&(&b.0, &b.1))
            })
            .map(|(_, _, agent)| agent)
    }

    pub fn other_mail(&self, current: &str) -> bool {
        self.loaded
            && self.summaries.values().any(|s| {
                s.unread_messages > 0
                    && !s.agent.eq_ignore_ascii_case(current)
                    && !self.pending.contains_key(&s.agent.to_ascii_lowercase())
            })
    }

    pub fn begin_read(&mut self, agent: &str, through: i64) -> bool {
        let key = agent.to_ascii_lowercase();
        if matches!(self.pending.get(&key), Some(Read::InFlight))
            || self.attempted.get(&key).is_some_and(|old| through <= *old)
        {
            return false;
        }
        self.attempted.insert(key.clone(), through);
        self.pending.insert(key.clone(), Read::InFlight);
        self.failed.remove(&key);
        true
    }

    pub fn acknowledged(&mut self, agent: &str, result: &Result<(), String>) {
        let key = agent.to_ascii_lowercase();
        if !matches!(self.pending.get(&key), Some(Read::InFlight)) {
            return;
        }
        if result.is_ok() {
            self.pending.insert(key, Read::AwaitRefresh);
            self.refresh = true;
        } else {
            self.pending.remove(&key);
            self.failed.insert(key);
        }
    }

    pub fn retry_failed(&mut self, agent: &str) {
        let key = agent.to_ascii_lowercase();
        if self.failed.remove(&key) {
            self.attempted.remove(&key);
        }
    }

    pub fn refresh_after_reads(&mut self) -> Vec<Request> {
        if self.refresh && !self.pending.values().any(|r| matches!(r, Read::InFlight)) {
            self.refresh = false;
            let requests = self.restart();
            for read in self.pending.values_mut() {
                if !matches!(read, Read::InFlight) {
                    *read = Read::Baseline(self.id);
                }
            }
            requests
        } else {
            Vec::new()
        }
    }

    pub fn restart(&mut self) -> Vec<Request> {
        let old = self.id;
        self.id = next_id();
        self.loaded = false;
        self.error = None;
        for read in self.pending.values_mut() {
            if matches!(read, Read::Baseline(_)) {
                *read = Read::Baseline(self.id);
            }
        }
        vec![Request::Unwatch(old), Request::WatchSummary(self.id)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(name: &str, time: &str) -> proto::AgentSummary {
        proto::AgentSummary {
            agent: name.into(),
            unread_messages: 1,
            oldest_unread_message: Some(time.into()),
            ..Default::default()
        }
    }
    #[test]
    fn history_is_a_typed_unique_recency_ring_and_skips_known_missing_targets() {
        let mut h = History::default();
        let a = Conversation::Agent("a".into());
        let b = Conversation::Agent("b".into());
        let group = Conversation::Group("a".into());
        assert_eq!(h.back(|_| true), None);
        for c in [&a, &b, &group, &a] {
            h.visit(c.clone());
        }
        assert_eq!(h.entries, [b.clone(), group.clone(), a.clone()]);
        for c in [&group, &b, &a, &group] {
            assert_eq!(h.back(|_| true), Some(c.clone()));
        }
        assert_eq!(h.entries.len(), 3);
        h.visit(Conversation::Agent("A".into()));
        assert_eq!(h.entries.len(), 3);
        assert_eq!(h.back(|c| *c != group), Some(b.clone()));
        assert_eq!(h.entries.len(), 2);
        assert_eq!(h.back(|_| false), None);
    }

    #[test]
    fn read_fence_ignores_stale_snapshots_coalesces_acks_and_keeps_new_mail() {
        let mut inbox = Inbox::default();
        let rows = vec![
            summary("Zed", "2026-10-18T12:00:00Z"),
            summary("Ada", "2026-10-18T13:00:00+01:00"),
        ];
        inbox.snapshot(inbox.id, &Ok(rows.clone()));
        assert_eq!(
            inbox.oldest().as_deref(),
            Some("Ada"),
            "equal instants tie by name"
        );
        assert!(inbox.begin_read("Ada", 12));
        assert_eq!(inbox.oldest().as_deref(), Some("Zed"));
        assert!(inbox.begin_read("Zed", 15));
        assert!(
            !inbox.begin_read("Ada", 13),
            "only one read per agent in flight"
        );
        inbox.acknowledged("Ada", &Ok(()));
        assert!(inbox.refresh_after_reads().is_empty());
        // A pre-ack stream can still deliver a queued stale snapshot.
        inbox.snapshot(inbox.id, &Ok(rows.clone()));
        assert_eq!(
            inbox.oldest(),
            None,
            "acknowledged mail stays suppressed until a fresh generation"
        );
        inbox.acknowledged("Zed", &Ok(()));
        let old = inbox.id;
        let refresh = inbox.refresh_after_reads();
        assert_eq!(
            refresh,
            [Request::Unwatch(old), Request::WatchSummary(inbox.id)]
        );
        assert!(!inbox.loaded);
        inbox.snapshot(old, &Ok(rows.clone()));
        assert!(!inbox.loaded);
        assert!(
            !inbox.begin_read("Ada", 12),
            "old local rows never restart reads"
        );
        // Even a new unread with the same timestamp must survive the fresh baseline.
        inbox.snapshot(inbox.id, &Ok(rows));
        assert_eq!(inbox.oldest().as_deref(), Some("Ada"));
        assert!(inbox.begin_read("Ada", 13));
    }

    #[test]
    fn failure_releases_suppression_without_retrying_stale_rows_and_reload_recovers() {
        let mut inbox = Inbox::default();
        let rows = vec![summary("A", "2026-10-18T12:00:00Z")];
        inbox.snapshot(inbox.id, &Ok(rows.clone()));
        assert!(inbox.begin_read("A", 1));
        inbox.acknowledged("A", &Err("offline".into()));
        assert_eq!(inbox.oldest().as_deref(), Some("A"));
        assert!(!inbox.begin_read("A", 1));
        assert!(inbox.refresh_after_reads().is_empty());
        inbox.retry_failed("A");
        assert!(inbox.begin_read("A", 1));
        inbox.acknowledged("A", &Ok(()));
        inbox.refresh_after_reads();
        let old = inbox.id;
        inbox.snapshot(old, &Err("unavailable".into()));
        assert!(!inbox.loaded);
        assert!(!inbox.other_mail("other"));
        inbox.restart();
        inbox.snapshot(old, &Ok(rows.clone()));
        assert!(!inbox.loaded);
        inbox.snapshot(inbox.id, &Ok(rows));
        assert!(inbox.loaded);
        assert!(inbox.other_mail("other"));
    }
}
