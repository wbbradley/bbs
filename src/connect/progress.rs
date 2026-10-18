//! Recent live activity evidence, independent of timeline filters and painting.

use std::time::{Duration, Instant};

use ratatui::text::Span;

use super::theme::{self, Role};

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

#[derive(Default)]
pub struct ProgressState {
    newest: Option<i64>,
    value: Progress,
    updated: Option<Instant>,
    closed: bool,
}

impl ProgressState {
    /// A latest-ID snapshot establishes history; it provides no live evidence.
    pub fn baseline(&mut self, newest: i64) {
        if self.newest.is_none() && !self.closed {
            self.newest = Some(newest);
        }
    }

    pub fn observe(&mut self, id: i64, record: &str, now: Instant) {
        let Some(newest) = self.newest else { return };
        if self.closed || id <= newest {
            return;
        }
        self.newest = Some(id);
        let parsed = serde_json::from_str::<serde_json::Value>(record).ok();
        self.value = match parsed.as_ref().and_then(|r| r["event"].as_str()) {
            Some("turn_end") => Progress::Done,
            Some("turn_error" | "stop" | "cancel") | None => Progress::Unknown,
            Some(_) => Progress::Progressing(match self.at(now) {
                Progress::Progressing(frame) => frame.wrapping_add(1),
                _ => 0,
            }),
        };
        self.updated = Some(now);
    }

    pub fn disconnect(&mut self) {
        self.closed = true;
        self.value = Progress::Unknown;
        self.updated = None;
    }

    pub fn at(&self, now: Instant) -> Progress {
        if self
            .updated
            .is_some_and(|updated| now.saturating_duration_since(updated) < Duration::from_secs(60))
        {
            self.value
        } else {
            Progress::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_duplicates_and_repaints_never_advance_or_refresh_activity() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        let text = r#"{"event":"text"}"#;
        p.observe(1, text, now);
        assert_eq!(p.at(now), Progress::Unknown);
        p.baseline(500);
        for id in 1..=500 {
            p.observe(id, text, now);
        }
        assert_eq!(p.at(now), Progress::Unknown);
        p.observe(501, text, now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        for _ in 0..10 {
            assert_eq!(
                p.at(now + Duration::from_secs(59)),
                Progress::Progressing(0)
            );
        }
        p.observe(501, text, now + Duration::from_secs(59));
        p.baseline(0);
        assert_eq!(p.at(now + Duration::from_secs(60)), Progress::Unknown);
        p.observe(502, text, now + Duration::from_secs(60));
        assert_eq!(
            p.at(now + Duration::from_secs(60)),
            Progress::Progressing(0)
        );
    }

    #[test]
    fn events_wrap_frames_end_turns_and_expire_without_sleeping() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        p.baseline(0);
        for id in 1..=257 {
            p.observe(id, r#"{"event":"tool"}"#, now);
        }
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.observe(258, r#"{"event":"turn_end"}"#, now);
        assert_eq!(p.at(now + Duration::from_secs(59)), Progress::Done);
        assert_eq!(p.at(now + Duration::from_secs(60)), Progress::Unknown);
        for (id, event) in [(259, "turn_error"), (260, "stop"), (261, "cancel")] {
            p.observe(id, &format!(r#"{{"event":"{event}"}}"#), now);
            assert_eq!(p.at(now), Progress::Unknown);
        }
        p.observe(262, r#"{"event":"text"}"#, now);
        assert_eq!(p.at(now), Progress::Progressing(0));
        p.disconnect();
        p.observe(263, r#"{"event":"text"}"#, now);
        assert_eq!(p.at(now), Progress::Unknown);
    }
}
