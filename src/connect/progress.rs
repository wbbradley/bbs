//! The conversation's last-known turn state from its activity stream: a
//! recorded turn end settles Done until a newer event arrives; only unfinished
//! progress carries a freshness timeout, measured from local receipt.

use std::time::{Duration, Instant};

use ratatui::text::Span;

use super::theme::{self, Role};

/// How long an unfinished event counts as fresh evidence.
const FRESH: Duration = Duration::from_secs(60);

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

/// Unfinished progress: the spinner frame and its local receipt time.
struct Fresh {
    frame: u8,
    at: Instant,
}

#[derive(Default)]
pub struct ProgressState {
    newest: Option<i64>,
    /// Done exactly when the newest known event is a recorded turn end.
    settled: Progress,
    /// Fresh unfinished progress, cleared by terminal events and disconnects.
    fresh: Option<Fresh>,
    closed: bool,
}

/// The settled state a record implies, when it is a terminal event.
fn terminal(record: &str) -> Option<Progress> {
    match serde_json::from_str::<serde_json::Value>(record)
        .ok()
        .as_ref()
        .and_then(|r| r["event"].as_str())
    {
        Some("turn_end") => Some(Progress::Done),
        Some("turn_error" | "stop" | "cancel") | None => Some(Progress::Unknown),
        Some(_) => None,
    }
}

impl ProgressState {
    /// A latest-row snapshot establishes history: a recorded turn end settles
    /// Done; anything else (or nothing) stays Unknown.
    pub fn baseline(&mut self, newest: i64, record: Option<&str>) {
        if self.newest.is_some() || self.closed {
            return;
        }
        self.newest = Some(newest);
        if let Some(settled) = record.and_then(terminal) {
            self.settled = settled;
        }
    }

    pub fn observe(&mut self, id: i64, record: &str, now: Instant) {
        let Some(newest) = self.newest else { return };
        if self.closed || id <= newest {
            return;
        }
        self.newest = Some(id);
        match terminal(record) {
            Some(settled) => {
                self.settled = settled;
                self.fresh = None;
            }
            None => {
                // Newer unfinished activity invalidates any earlier
                // completion; only while fresh does it show progress.
                self.settled = Progress::Unknown;
                let frame = self
                    .fresh
                    .as_ref()
                    .map_or(0, |fresh| fresh.frame.wrapping_add(1));
                self.fresh = Some(Fresh { frame, at: now });
            }
        }
    }

    /// The stream ended; a recorded completion still stands.
    pub fn disconnect(&mut self) {
        self.closed = true;
        self.fresh = None;
    }

    pub fn at(&self, now: Instant) -> Progress {
        if let Some(fresh) = &self.fresh
            && now.saturating_duration_since(fresh.at) < FRESH
        {
            return Progress::Progressing(fresh.frame);
        }
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(event: &str) -> String {
        format!(r#"{{"event":"{event}"}}"#)
    }

    #[test]
    fn recorded_turn_end_settles_done_until_a_newer_event() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        // History ending in turn_end opens Done and never expires.
        p.baseline(500, Some(&record("turn_end")));
        assert_eq!(p.at(now), Progress::Done);
        assert_eq!(p.at(now + Duration::from_secs(3600)), Progress::Done);
        // Newer unfinished activity shows progress while fresh...
        p.observe(501, &record("text"), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        // ...but once it times out the older completion must not return.
        assert_eq!(p.at(now + Duration::from_secs(60)), Progress::Unknown);
        assert_eq!(p.at(now + Duration::from_secs(3600)), Progress::Unknown);
        // A recorded failure or a new turn end resettles the state.
        p.observe(502, &record("turn_end"), now);
        assert_eq!(p.at(now + Duration::from_secs(3600)), Progress::Done);
        p.observe(503, &record("turn_error"), now);
        assert_eq!(p.at(now), Progress::Unknown);
    }

    #[test]
    fn unfinished_history_stays_unknown() {
        let now = Instant::now();
        let mut fresh = ProgressState::default();
        fresh.baseline(9, Some(&record("text")));
        assert_eq!(fresh.at(now), Progress::Unknown);
        let mut none = ProgressState::default();
        none.baseline(0, None);
        assert_eq!(none.at(now), Progress::Unknown);
    }

    #[test]
    fn history_duplicates_and_repaints_never_advance_or_refresh_activity() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        // Nothing observed before the baseline.
        p.observe(1, &record("text"), now);
        assert_eq!(p.at(now), Progress::Unknown);
        p.baseline(500, None);
        for id in 1..=500 {
            p.observe(id, &record("text"), now);
        }
        assert_eq!(p.at(now), Progress::Unknown);
        // A second baseline does not move.
        p.observe(501, &record("text"), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.baseline(0, Some(&record("turn_end")));
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(501, &record("text"), now);
        assert_eq!(p.at(now), Progress::Progressing(0));
    }

    #[test]
    fn progress_is_fresh_from_receipt_for_a_minute() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        p.baseline(0, None);
        p.observe(1, &record("text"), now);
        assert_eq!(p.at(now + Duration::from_secs(59)), Progress::Progressing(0));
        assert_eq!(p.at(now + Duration::from_secs(60)), Progress::Unknown);
    }

    #[test]
    fn events_wrap_frames_and_disconnect_keeps_recorded_completion() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        p.baseline(0, None);
        for id in 1..=257 {
            p.observe(id, &record("tool"), now);
        }
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(258, &record("turn_end"), now);
        assert_eq!(p.at(now + Duration::from_secs(3600)), Progress::Done);
        p.disconnect();
        assert_eq!(p.at(now), Progress::Done);
        p.observe(259, &record("text"), now);
        assert_eq!(p.at(now), Progress::Done, "closed stays closed");
        // Newer unfinished activity already replaced the completion, so a
        // disconnect now leaves unknown.
        let mut q = ProgressState::default();
        q.baseline(1, Some(&record("turn_end")));
        q.observe(2, &record("text"), now);
        q.disconnect();
        assert_eq!(q.at(now), Progress::Unknown);
        // Without a recorded end, a disconnect drops fresh progress.
        let mut r = ProgressState::default();
        r.baseline(0, None);
        r.observe(1, &record("text"), now);
        assert_eq!(r.at(now), Progress::Progressing(0));
        r.disconnect();
        assert_eq!(r.at(now), Progress::Unknown);
    }
}
