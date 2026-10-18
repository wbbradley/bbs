//! The conversation's last-known turn state from its activity stream: a
//! recorded turn end settles Done until a newer event arrives; only unfinished
//! progress carries a freshness timeout, measured from local receipt.

use std::time::{Duration, Instant};

use ratatui::text::Span;

use super::theme::{self, Role};

/// How long an unfinished event counts as fresh evidence.
const FRESH: Duration = Duration::from_secs(60);
/// Frames advance every 100ms for this long, then every 500ms.
const FAST: Duration = Duration::from_secs(3);

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
            Self::Unknown => ("·", Role::Muted),
        };
        Span::styled(glyph, theme::style(role))
    }
}

/// Unfinished progress: `received` drives the fast window and the freshness
/// expiry; `clock` is the animation's virtual time (real time while fast, a
/// fifth of it while slow), so events never reset or freeze the phase.
struct Fresh {
    received: Instant,
    clock: Duration,
    set: Instant,
}

/// The virtual animation time at `now`.
fn clock(fresh: &Fresh, now: Instant) -> Duration {
    let fast_end = fresh.received + FAST;
    let mut clock = fresh.clock;
    let mut at = fresh.set;
    if at < fast_end {
        let end = now.min(fast_end);
        clock += end - at;
        at = end;
    }
    if now > fast_end && now > at {
        clock += (now - fast_end.max(at)) / 5;
    }
    clock
}

/// The displayed frame: one per 100ms of animation time.
fn frames(fresh: &Fresh, now: Instant) -> u32 {
    (clock(fresh, now).as_millis() / 100) as u32
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
                // completion; only while fresh does it show progress. The
                // animation phase carries over, only its speed windows move.
                self.settled = Progress::Unknown;
                let clock = self
                    .fresh
                    .as_ref()
                    .map_or(Duration::ZERO, |fresh| clock(fresh, now));
                self.fresh = Some(Fresh {
                    received: now,
                    clock,
                    set: now,
                });
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
            && now.saturating_duration_since(fresh.received) < FRESH
        {
            return Progress::Progressing(frames(fresh, now) as u8);
        }
        self.settled
    }

    /// When the displayed frame next changes, while progress is fresh.
    pub fn next_frame_at(&self, now: Instant) -> Option<Instant> {
        let fresh = self.fresh.as_ref()?;
        let elapsed = now.saturating_duration_since(fresh.received);
        if elapsed >= FRESH {
            return None;
        }
        let at_clock = clock(fresh, now);
        let mut remaining =
            Duration::from_millis(100) * ((at_clock.as_millis() / 100) as u32 + 1) - at_clock;
        let fast_end = fresh.received + FAST;
        let mut at = now;
        // Clock time accrues in real time inside the fast window, then at a
        // fifth of it, so crossing the boundary splits the wait.
        if at < fast_end {
            let fast_room = fast_end - at;
            if fast_room >= remaining {
                return Some((at + remaining).min(fresh.received + FRESH));
            }
            remaining -= fast_room;
            at = fast_end;
        }
        Some((at + remaining * 5).min(fresh.received + FRESH))
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
    fn progress_frames_follow_elapsed_time_until_expiry() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        p.baseline(0, None);
        p.observe(1, &record("text"), now);
        // 100ms frames for the first three seconds.
        assert_eq!(
            p.at(now + Duration::from_millis(99)),
            Progress::Progressing(0)
        );
        assert_eq!(
            p.at(now + Duration::from_millis(100)),
            Progress::Progressing(1)
        );
        assert_eq!(
            p.at(now + Duration::from_millis(2900)),
            Progress::Progressing(29)
        );
        // Then 500ms frames.
        assert_eq!(
            p.at(now + Duration::from_millis(3000)),
            Progress::Progressing(30)
        );
        assert_eq!(
            p.at(now + Duration::from_millis(3499)),
            Progress::Progressing(30)
        );
        assert_eq!(
            p.at(now + Duration::from_millis(3500)),
            Progress::Progressing(31)
        );
        assert_eq!(
            p.at(now + Duration::from_millis(59999)),
            Progress::Progressing(143)
        );
        assert_eq!(p.at(now + Duration::from_secs(60)), Progress::Unknown);
    }

    #[test]
    fn frequent_events_keep_the_phase_moving() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        p.baseline(0, None);
        // Ten events inside 500ms: the spinner advances with time, not events.
        for i in 1..=10_u64 {
            p.observe(
                i as i64,
                &record("text"),
                now + Duration::from_millis(50 * i),
            );
        }
        // The clock starts at the first event (50ms in), so four full frames.
        assert_eq!(
            p.at(now + Duration::from_millis(500)),
            Progress::Progressing(4)
        );
        // A slow-phase arrival switches speed without a backward jump:
        // 0.5s..3.5s fast (3000ms) plus 3.5s..10s slow (1300ms) on top of
        // 450ms = 4750ms, so frame 47 continues.
        p.observe(11, &record("text"), now + Duration::from_secs(10));
        assert_eq!(
            p.at(now + Duration::from_secs(10)),
            Progress::Progressing(47)
        );
        assert_eq!(
            p.at(now + Duration::from_secs(10) + Duration::from_millis(100)),
            Progress::Progressing(48)
        );
        assert_eq!(
            p.at(now + Duration::from_secs(10) + Duration::from_millis(499)),
            Progress::Progressing(52)
        );
        // Freshness follows the newest receipt.
        assert_eq!(
            p.at(now + Duration::from_secs(10) + Duration::from_secs(60)),
            Progress::Unknown
        );
    }

    #[test]
    fn next_frame_tracks_boundaries_and_stops_at_expiry() {
        let now = Instant::now();
        let mut p = ProgressState::default();
        assert_eq!(p.next_frame_at(now), None, "nothing observed");
        p.baseline(0, Some(&record("turn_end")));
        assert_eq!(p.next_frame_at(now), None, "settled does not animate");
        let mut p = ProgressState::default();
        p.baseline(0, None);
        p.observe(1, &record("text"), now);
        assert_eq!(p.next_frame_at(now), Some(now + Duration::from_millis(100)));
        assert_eq!(
            p.next_frame_at(now + Duration::from_millis(100)),
            Some(now + Duration::from_millis(200))
        );
        assert_eq!(
            p.next_frame_at(now + Duration::from_secs(3)),
            Some(now + Duration::from_millis(3500))
        );
        assert_eq!(
            p.next_frame_at(now + Duration::from_millis(59950)),
            Some(now + Duration::from_secs(60))
        );
        assert_eq!(p.next_frame_at(now + Duration::from_secs(60)), None);
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
