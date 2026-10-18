//! The conversation's last-known turn state from its activity stream: recorded
//! terminal events are trusted history that survives expiry and disconnects;
//! only unfinished progress carries a freshness timeout.

use chrono::{DateTime, Utc};
use ratatui::text::Span;

use super::theme::{self, Role};

/// How long an unfinished-activity event counts as fresh evidence, measured
/// from the event's stored time, never from when it happened to arrive.
const FRESH: chrono::Duration = chrono::Duration::seconds(60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Progress {
    Progressing(u8),
    Done,
    #[default]
    Unknown,
}

impl Progress {
    pub fn indicator(self) -> Span<'static> {
        let (glyph, role) = match self {
            Self::Progressing(frame) => (
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][usize::from(frame) % 8],
                Role::Accent,
            ),
            Self::Done => ("✓", Role::Success),
            Self::Unknown => ("?", Role::Muted),
        };
        Span::styled(glyph, theme::style(role))
    }
}

/// Unfinished progress: the spinner frame and the event's stored time.
struct Fresh {
    frame: u8,
    at: DateTime<Utc>,
}

#[derive(Default)]
pub struct ProgressState {
    newest: Option<i64>,
    /// Recorded completion: terminal events stick until a later event, and
    /// survive freshness expiry and disconnects.
    settled: Progress,
    /// Fresh unfinished progress, cleared by terminal events and disconnects.
    fresh: Option<Fresh>,
    closed: bool,
}

/// One activity record: its terminal effect, and its time when nonterminal.
fn classify(record: &str) -> (Option<Progress>, Option<DateTime<Utc>>) {
    let parsed = serde_json::from_str::<serde_json::Value>(record).ok();
    let ts = parsed
        .as_ref()
        .and_then(|r| r["ts"].as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    let settled = match parsed.as_ref().and_then(|r| r["event"].as_str()) {
        Some("turn_end") => Some(Progress::Done),
        Some("turn_error" | "stop" | "cancel") => Some(Progress::Unknown),
        Some(_) => None,
        None => Some(Progress::Unknown),
    };
    let nonterminal = ts.filter(|_| settled.is_none());
    (settled, nonterminal)
}

impl ProgressState {
    /// A latest-row snapshot establishes history: a recorded terminal event
    /// settles the state; a fresh unfinished event counts as live evidence,
    /// an old one stays unknown.
    pub fn baseline(&mut self, newest: i64, record: Option<&str>, now: DateTime<Utc>) {
        if self.newest.is_some() || self.closed {
            return;
        }
        self.newest = Some(newest);
        if let Some(record) = record {
            match classify(record) {
                (Some(settled), _) => self.settled = settled,
                (None, Some(at)) if now.signed_duration_since(at) < FRESH => {
                    self.fresh = Some(Fresh { frame: 0, at });
                }
                (None, _) => {}
            }
        }
    }

    pub fn observe(&mut self, id: i64, record: &str, now: DateTime<Utc>) {
        let Some(newest) = self.newest else { return };
        if self.closed || id <= newest {
            return;
        }
        self.newest = Some(id);
        match classify(record) {
            (Some(settled), _) => {
                self.settled = settled;
                self.fresh = None;
            }
            (None, at) => {
                let frame = match self.at(now) {
                    Progress::Progressing(frame) => frame.wrapping_add(1),
                    _ => 0,
                };
                self.fresh = at.map(|at| Fresh { frame, at });
            }
        }
    }

    /// The stream ended; recorded completion still stands.
    pub fn disconnect(&mut self) {
        self.closed = true;
        self.fresh = None;
    }

    pub fn at(&self, now: DateTime<Utc>) -> Progress {
        if let Some(fresh) = &self.fresh
            && now.signed_duration_since(fresh.at) < FRESH
        {
            return Progress::Progressing(fresh.frame);
        }
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn record(event: &str, at: DateTime<Utc>) -> String {
        format!(r#"{{"event":"{event}","ts":"{}"}}"#, at.to_rfc3339())
    }

    #[test]
    fn recorded_turn_end_settles_done_without_expiring() {
        let now = Utc::now();
        let mut p = ProgressState::default();
        p.baseline(500, Some(&record("turn_end", now - Duration::hours(2))), now);
        assert_eq!(p.at(now), Progress::Done);
        assert_eq!(p.at(now + Duration::hours(1)), Progress::Done);
        // A later event supersedes it; a recorded failure clears it.
        p.observe(501, &record("text", now), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(502, &record("turn_end", now), now);
        assert_eq!(p.at(now + Duration::hours(1)), Progress::Done);
        p.observe(503, &record("turn_error", now), now);
        assert_eq!(p.at(now), Progress::Unknown);
        assert_eq!(p.at(now + Duration::hours(1)), Progress::Unknown);
    }

    #[test]
    fn unfinished_history_is_fresh_only_by_its_stored_time() {
        let now = Utc::now();
        let mut fresh = ProgressState::default();
        fresh.baseline(9, Some(&record("text", now)), now);
        assert_eq!(fresh.at(now), Progress::Progressing(0));
        assert_eq!(fresh.at(now + Duration::seconds(59)), Progress::Progressing(0));
        assert_eq!(fresh.at(now + Duration::seconds(60)), Progress::Unknown);
        let mut stale = ProgressState::default();
        stale.baseline(9, Some(&record("text", now - Duration::minutes(5))), now);
        assert_eq!(stale.at(now), Progress::Unknown);
        let mut none = ProgressState::default();
        none.baseline(0, None, now);
        assert_eq!(none.at(now), Progress::Unknown);
    }

    #[test]
    fn history_duplicates_and_repaints_never_advance_or_refresh_activity() {
        let now = Utc::now();
        let mut p = ProgressState::default();
        // Nothing observed before the baseline.
        p.observe(1, &record("text", now), now);
        assert_eq!(p.at(now), Progress::Unknown);
        p.baseline(500, None, now);
        for id in 1..=500 {
            p.observe(id, &record("text", now), now);
        }
        assert_eq!(p.at(now), Progress::Unknown);
        // A second baseline does not move.
        p.observe(501, &record("text", now), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.baseline(0, Some(&record("turn_end", now)), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(501, &record("text", now), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
    }

    #[test]
    fn events_wrap_frames_and_disconnect_keeps_recorded_completion() {
        let now = Utc::now();
        let mut p = ProgressState::default();
        p.baseline(0, None, now);
        for id in 1..=257 {
            p.observe(id, &record("tool", now), now);
        }
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(258, &record("turn_end", now), now);
        assert_eq!(p.at(now + Duration::hours(1)), Progress::Done);
        p.disconnect();
        assert_eq!(p.at(now), Progress::Done);
        p.observe(259, &record("text", now), now);
        assert_eq!(p.at(now), Progress::Done, "closed stays closed");
        // Without a recorded end, a disconnect drops fresh progress.
        let mut q = ProgressState::default();
        q.baseline(0, None, now);
        q.observe(1, &record("text", now), now);
        assert_eq!(q.at(now), Progress::Progressing(0));
        q.disconnect();
        assert_eq!(q.at(now), Progress::Unknown);
    }
}
