//! Timer schedules (timers.v1): once (`in 30m`, `at <RFC3339>`) or recurring
//! (`every 1h`, or a 5-field cron expression in UTC). Every time is computed
//! from an explicit `now`, so callers and tests choose the clock.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use croner::Cron;

/// Upcoming runs a recurring schedule's shortest gap is measured over.
const GAP_RUNS: usize = 50;

#[derive(Clone, Debug)]
pub enum Schedule {
    Once(DateTime<Utc>),
    Every(Duration),
    Cron(Box<Cron>),
}

impl Schedule {
    /// Parse `when` as of `now`.
    pub fn parse(when: &str, now: DateTime<Utc>) -> Result<Schedule> {
        let when = when.trim();
        if let Some(after) = when.strip_prefix("in ") {
            return Ok(Schedule::Once(now + duration(after)?));
        }
        if let Some(at) = when.strip_prefix("at ") {
            let at = DateTime::parse_from_rfc3339(at.trim())
                .with_context(|| format!("invalid time {:?}; use RFC 3339", at.trim()))?
                .with_timezone(&Utc);
            if at <= now {
                bail!("{} is in the past", at.to_rfc3339());
            }
            return Ok(Schedule::Once(at));
        }
        if let Some(every) = when.strip_prefix("every ") {
            return Ok(Schedule::Every(duration(every)?));
        }
        if when.split_whitespace().count() == 5 {
            let cron = Cron::from_str(when)
                .map_err(|e| anyhow::anyhow!("invalid cron expression {when:?}: {e}"))?;
            return Ok(Schedule::Cron(Box::new(cron)));
        }
        bail!(
            "invalid schedule {when:?}: use \"in 30m\", \"at <RFC 3339 time>\", \"every 1h\", or a 5-field cron expression (UTC)"
        )
    }

    /// A recurring schedule as stored and shown: `every <secs>s` or the cron
    /// expression. `None` for a one-time timer.
    pub fn repeat(&self) -> Option<String> {
        match self {
            Schedule::Once(_) => None,
            Schedule::Every(d) => Some(format!("every {}", humantime::format_duration(*d))),
            Schedule::Cron(cron) => Some(cron.pattern.to_string()),
        }
    }

    /// A stored `repeat` back as a schedule.
    pub fn from_repeat(repeat: &str) -> Result<Schedule> {
        Schedule::parse(repeat, Utc::now())
    }

    /// The first run after `now`, or for a one-time timer its time.
    pub fn first(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        match self {
            Schedule::Once(at) => Ok(*at),
            _ => self.next_after(now).context("the schedule never runs"),
        }
    }

    /// The next run after `now`; `None` for a one-time timer, which is done.
    pub fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Once(_) => None,
            Schedule::Every(d) => Some(now + *d),
            Schedule::Cron(cron) => cron.find_next_occurrence(&now, false).ok(),
        }
    }

    /// The shortest time between two runs; `None` for a one-time timer.
    pub fn shortest_gap(&self, now: DateTime<Utc>) -> Option<Duration> {
        match self {
            Schedule::Once(_) => None,
            Schedule::Every(d) => Some(*d),
            Schedule::Cron(cron) => cron
                .iter_after(now)
                .take(GAP_RUNS)
                .collect::<Vec<_>>()
                .windows(2)
                .filter_map(|w| (w[1] - w[0]).to_std().ok())
                .min(),
        }
    }
}

fn duration(text: &str) -> Result<Duration> {
    let d = humantime::parse_duration(text.trim())
        .with_context(|| format!("invalid duration {:?}; use e.g. 30m, 1h, 2d", text.trim()))?;
    if d.is_zero() {
        bail!("the duration must be more than zero");
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn one_time_schedules_run_once() {
        let now = at("2026-10-16T12:00:00Z");
        let s = Schedule::parse("in 30m", now).unwrap();
        assert_eq!(s.first(now).unwrap(), at("2026-10-16T12:30:00Z"));
        assert!(s.next_after(at("2026-10-16T12:30:00Z")).is_none());
        assert_eq!(s.repeat(), None);
        let s = Schedule::parse("at 2026-10-17T09:00:00+02:00", now).unwrap();
        assert_eq!(s.first(now).unwrap(), at("2026-10-17T07:00:00Z"));
        let past = Schedule::parse("at 2026-10-16T11:00:00Z", now).unwrap_err();
        assert!(past.to_string().contains("in the past"), "{past}");
    }

    #[test]
    fn recurring_schedules_resume_from_now() {
        let now = at("2026-10-16T12:00:00Z");
        let every = Schedule::parse("every 1h", now).unwrap();
        assert_eq!(every.first(now).unwrap(), at("2026-10-16T13:00:00Z"));
        // A run missed for a day comes next an hour after it fires, not 24 times.
        let late = at("2026-10-17T12:10:00Z");
        assert_eq!(every.next_after(late).unwrap(), at("2026-10-17T13:10:00Z"));
        assert_eq!(every.repeat().as_deref(), Some("every 1h"));

        let cron = Schedule::parse("0 9 * * 1-5", now).unwrap();
        // 2026-10-16 is a Friday; the next weekday 09:00 UTC is Monday.
        assert_eq!(cron.first(now).unwrap(), at("2026-10-19T09:00:00Z"));
        assert_eq!(cron.repeat().as_deref(), Some("0 9 * * 1-5"));
        let stored = Schedule::from_repeat(&cron.repeat().unwrap()).unwrap();
        assert_eq!(stored.next_after(now), cron.next_after(now));
    }

    #[test]
    fn shortest_gaps_catch_uneven_cron_expressions() {
        let now = at("2026-10-16T12:00:00Z");
        let gap = |when: &str| Schedule::parse(when, now).unwrap().shortest_gap(now);
        assert_eq!(gap("every 10m"), Some(Duration::from_secs(600)));
        assert_eq!(gap("*/15 * * * *"), Some(Duration::from_secs(900)));
        // Mostly an hour apart, but minutes 0 and 1 are one minute apart.
        assert_eq!(gap("0,1 * * * *"), Some(Duration::from_secs(60)));
        assert_eq!(gap("in 1m"), None);
    }

    #[test]
    fn bad_schedules_say_what_is_accepted() {
        let now = Utc::now();
        for (when, why) in [
            ("tomorrow", "use \"in 30m\""),
            ("in soon", "invalid duration"),
            ("every 0s", "more than zero"),
            ("at noon", "use RFC 3339"),
            ("61 * * * *", "invalid cron expression"),
            ("* * * * * *", "use \"in 30m\""),
        ] {
            let err = format!("{:#}", Schedule::parse(when, now).unwrap_err());
            assert!(err.contains(why), "{when}: {err}");
        }
    }
}
