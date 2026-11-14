//! How long calls wait for and hold the server's one database connection.
//! A call that holds it long is logged at once; every minute the labels that
//! held it longest in total are logged, so a frequent cheap call shows too.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A single call holding the connection this long is logged by itself.
const SLOW_HOLD: Duration = Duration::from_millis(250);
/// How often the busiest labels are logged.
const SUMMARY_EVERY: Duration = Duration::from_secs(60);
/// How many labels a summary names.
const SUMMARY_TOP: usize = 8;

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

/// Recorded when dropped, so a call that fails or panics is counted too.
pub(super) struct Record {
    pub(super) label: Label,
    pub(super) waited: Duration,
    pub(super) held: Instant,
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
        let held = self.held.elapsed();
        let label = self.label.to_string();
        if held >= SLOW_HOLD {
            tracing::warn!(
                call = %label,
                held_ms = held.as_millis() as u64,
                waited_ms = self.waited.as_millis() as u64,
                "slow database call"
            );
        }
        let mut window = WINDOW.lock().unwrap_or_else(PoisonError::into_inner);
        let window = window.get_or_insert_with(|| Window {
            started: Instant::now(),
            by_label: HashMap::new(),
        });
        let totals = window.by_label.entry(label).or_default();
        totals.calls += 1;
        totals.held += held;
        totals.waited += self.waited;
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
