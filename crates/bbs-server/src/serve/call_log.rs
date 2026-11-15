//! How long calls wait for and hold the server's one database connection.
//! A call that holds it long is logged at once; every minute the labels that
//! held it longest in total are logged, so a frequent cheap call shows too.
//! The summary is logged by the first call after the minute, so a quiet
//! server logs nothing. The integration stream's authentication uses its own
//! read-only connection and is not recorded here.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A single call holding the connection this long is logged by itself.
const SLOW_HOLD: Duration = Duration::from_millis(250);
/// How often the busiest labels are logged.
const SUMMARY_EVERY: Duration = Duration::from_secs(60);
/// How many labels a summary names.
const SUMMARY_TOP: usize = 12;
/// The most distinct labels a window keeps.
const MAX_LABELS: usize = 512;

/// What a call is: an RPC by name, or the code location of any other call.
pub(super) enum Label {
    Rpc(String),
    At(&'static Location<'static>),
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Label::Rpc(rpc) => f.write_str(rpc),
            Label::At(at) => write!(f, "{}:{}", at.file(), at.line()),
        }
    }
}

/// One call's use of the connection, recorded when dropped so a call that
/// fails or panics is counted too. Declare it before the connection guard:
/// the guard then drops first, and the logging here never holds the
/// connection.
pub(super) struct Record {
    label: Label,
    /// Who called and with what, when the call says so.
    detail: Option<String>,
    queued: Instant,
    locked: Option<Instant>,
}

impl Record {
    /// Start timing a call queued at `queued`. Its wait covers getting a
    /// blocking thread as well as the connection.
    pub(super) fn new(label: Label, queued: Instant) -> Self {
        Record {
            label,
            detail: None,
            queued,
            locked: None,
        }
    }

    /// The call now holds the connection.
    pub(super) fn locked(&mut self) {
        self.locked = Some(Instant::now());
    }

    /// Name who called and with what, so the log can tell callers apart.
    pub(super) fn detail(&mut self, detail: String) {
        self.detail = Some(detail);
    }
}

#[derive(Default)]
struct Totals {
    calls: u64,
    held: Duration,
    waited: Duration,
}

struct Window {
    started: Instant,
    by_label: HashMap<String, Totals>,
}

static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

impl Drop for Record {
    fn drop(&mut self) {
        let Some(locked) = self.locked else {
            return;
        };
        let held = locked.elapsed();
        let waited = locked - self.queued;
        let label = match &self.detail {
            Some(detail) => format!("{} {detail}", self.label),
            None => self.label.to_string(),
        };
        if held >= SLOW_HOLD {
            tracing::warn!(
                call = %label,
                held_ms = held.as_millis() as u64,
                waited_ms = waited.as_millis() as u64,
                "slow database call"
            );
        }
        let mut window = WINDOW.lock().unwrap_or_else(PoisonError::into_inner);
        let window = window.get_or_insert_with(|| Window {
            started: Instant::now(),
            by_label: HashMap::new(),
        });
        // Callers named in labels are many, so past the cap the rest of the
        // window is counted together.
        let label = if window.by_label.len() >= MAX_LABELS && !window.by_label.contains_key(&label)
        {
            "other".to_string()
        } else {
            label
        };
        let totals = window.by_label.entry(label).or_default();
        totals.calls += 1;
        totals.held += held;
        totals.waited += waited;
        let elapsed = window.started.elapsed();
        if elapsed >= SUMMARY_EVERY {
            summarize(elapsed, std::mem::take(&mut window.by_label));
            window.started = Instant::now();
        }
    }
}

fn summarize(elapsed: Duration, by_label: HashMap<String, Totals>) {
    let held: Duration = by_label.values().map(|t| t.held).sum();
    let calls: u64 = by_label.values().map(|t| t.calls).sum();
    let mut busiest: Vec<_> = by_label.into_iter().collect();
    busiest.sort_by_key(|(_, t)| std::cmp::Reverse(t.held));
    let top = busiest
        .iter()
        .take(SUMMARY_TOP)
        .map(|(label, t)| {
            format!(
                "{label} {}x held {}ms waited {}ms",
                t.calls,
                t.held.as_millis(),
                t.waited.as_millis()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    tracing::info!(
        window_s = elapsed.as_secs(),
        calls,
        held_pct = (held.as_secs_f64() * 100.0 / elapsed.as_secs_f64()).round() as u64,
        "database connection use: {top}"
    );
}
