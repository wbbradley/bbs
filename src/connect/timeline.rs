//! A thread's timeline: messages and, in an agent's thread, the activity it
//! shares, drawn as `bbs headless logs --pretty` draws it, in time order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use chrono::DateTime;
use ratatui::style::Style;
use ratatui::text::Line;

use super::app::next_id;
use super::event::{Data, Request};
use super::progress::{Progress, ProgressState};
use super::theme;
use crate::backend::AGENT_ACTIVITY;
use crate::proto::{self, Message};
use crate::ui::activity::{Activity, ToolLine};
use crate::ui::message::{compact_message_lines, find_matches, message_lines_with_recipient};
use crate::ui::thread::Row;

/// Events each batch holds; the stream catches up from the start in batches.
const BACKLOG: u32 = 200;
/// Activity records kept; the oldest go first.
pub(crate) const MAX_RECORDS: usize = 5000;

/// One row of a timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Message {
        message: Message,
        /// Mail between the viewer and the agent omits its header; everyone
        /// else's keeps it, so the sender stays identified.
        headerless: bool,
        recipient: Option<proto::Kind>,
        outgoing: bool,
        /// A possible gap in the raw feed before this visible message.
        gap: bool,
    },
    /// One activity record, drawn when it arrived.
    Stream {
        id: i64,
        created_at: String,
        lines: Vec<Line<'static>>,
        /// A finished tool call, kept whole and drawn as one line fitted to
        /// the width; runs of these sit together after one blank.
        call: Option<ToolLine>,
    },
}

impl Entry {
    fn is_tool(&self) -> bool {
        matches!(self, Entry::Stream { call: Some(_), .. })
    }
}

/// Whether tool calls after `prev` (none: the top) start a new run, and so
/// take one blank line first: `prev` is no tool call and does not already
/// end with a blank line.
pub fn starts_tool_run(prev: Option<&Entry>) -> bool {
    let blank = |line: &Line| {
        line.spans
            .iter()
            .all(|s| s.content.chars().all(char::is_whitespace))
    };
    prev.is_none_or(|p| !p.is_tool() && p.lines(false, None).last().is_none_or(|l| !blank(l)))
}

/// Microseconds since the epoch, so rows from the two sources interleave.
fn micros(time: &str) -> i64 {
    DateTime::parse_from_rfc3339(time).map_or(0, |t| t.timestamp_micros())
}

impl Row for Entry {
    /// Time, then messages before activity, then each source's own order.
    type Key = (i64, u8, i64);
    const MERGES: bool = true;

    fn key(&self) -> Self::Key {
        match self {
            Entry::Message { message: m, .. } => (micros(&m.created_at), 0, m.id),
            Entry::Stream { id, created_at, .. } => (micros(created_at), 1, *id),
        }
    }

    fn lines(&self, gap: bool, query: Option<&str>) -> Vec<Line<'_>> {
        match self {
            Entry::Message {
                message,
                headerless,
                recipient,
                outgoing,
                gap: raw_gap,
            } => {
                let mut lines = if *headerless {
                    compact_message_lines(message, gap || *raw_gap, query)
                } else {
                    message_lines_with_recipient(message, gap || *raw_gap, query, *recipient)
                };
                if *outgoing {
                    if *headerless {
                        lines.insert(1 + usize::from(gap || *raw_gap), Line::default());
                    }
                    for line in &mut lines {
                        line.style = theme::style(theme::Role::HumanSurface).patch(line.style);
                    }
                    lines.push(Line::default());
                }
                lines
            }
            Entry::Stream { lines, .. } => lines.clone(),
        }
    }

    fn style(&self) -> Style {
        match self {
            Entry::Message { outgoing: true, .. } => theme::style(theme::Role::HumanSurface),
            _ => Style::default(),
        }
    }

    fn separator_style(&self) -> Option<Style> {
        matches!(
            self,
            Entry::Message {
                headerless: true,
                outgoing: true,
                ..
            }
        )
        .then(|| theme::style(theme::Role::Screen))
    }

    /// A tool call fits on one line of the width; search still sees all of it.
    fn lines_at(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        match self {
            Entry::Stream {
                call: Some(call), ..
            } => call.lines(Some(usize::from(width))),
            _ => self.lines(gap, query),
        }
    }

    /// A blank line before each run of tool calls, the first included,
    /// unless the row before already ends with one; none within a run.
    fn spaced_after(&self, prev: Option<&Self>) -> bool {
        self.is_tool() && starts_tool_run(prev)
    }

    fn matches(&self, query: &str) -> bool {
        match self {
            Entry::Message { message: m, .. } => m.matches(query),
            Entry::Stream { lines, .. } => lines.iter().any(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                !find_matches(&text, query).is_empty()
            }),
        }
    }
}

/// What an agent's timeline shows; `A` cycles through them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Show {
    #[default]
    All,
    Messages,
    Stream,
}

impl Show {
    pub fn next(self) -> Show {
        match self {
            Show::All => Show::Messages,
            Show::Messages => Show::Stream,
            Show::Stream => Show::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Show::All => "all",
            Show::Messages => "messages",
            Show::Stream => "activity",
        }
    }

    pub fn messages(self) -> bool {
        self != Show::Stream
    }

    pub fn stream(self) -> bool {
        self != Show::Messages
    }
}

/// Which activity events show; `E` cycles through them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Kinds {
    #[default]
    All,
    /// Without the agent's text and thoughts: turns, mail and tools.
    NoText,
    /// Errors and warnings only.
    Problems,
}

impl Kinds {
    pub fn next(self) -> Kinds {
        match self {
            Kinds::All => Kinds::NoText,
            Kinds::NoText => Kinds::Problems,
            Kinds::Problems => Kinds::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kinds::All => "all kinds",
            Kinds::NoText => "no text",
            Kinds::Problems => "problems",
        }
    }

    fn shows(self, event: &str) -> bool {
        match self {
            Kinds::All => true,
            Kinds::NoText => !matches!(event, "text" | "thought"),
            Kinds::Problems => matches!(event, "warning" | "error" | "turn_error" | "stop"),
        }
    }
}

/// An agent's shared activity, as it streams in.
pub struct Stream {
    id: u64,
    agent: String,
    pub kinds: Kinds,
    /// Every record kept, so another filter can redraw them all.
    records: VecDeque<Kept>,
    renderer: Activity,
    /// A batch has arrived, so no records means nothing is shared.
    pub loaded: bool,
    /// The stream's last error; it has ended until a reload.
    pub error: Option<String>,
    progress: ProgressState,
    /// Delivered-as ids drawn since the last full redraw, so a retried copy
    /// of the same text draws once.
    drawn: HashSet<i64>,
    /// Evicted records named delivered mail: the timeline must redraw from
    /// what is kept, so its rows and `drawn` agree and stay bounded.
    redraw: bool,
}

/// The messages a thread holds, by id: true for an automatic reply from the
/// thread's agent, the only mail a `text` record's `mail_id` may stand for.
/// Empty while messages are filtered out, so activity then stands in.
pub type Loaded = HashMap<i64, bool>;

/// A kept record and the messages it names: the owner mail its text was
/// delivered as (`mail_id`), and the mail a receipt or turn echoes
/// (`mail_ids`).
struct Kept {
    activity: proto::Activity,
    mail_id: Option<i64>,
    mail_ids: Vec<i64>,
}

impl Kept {
    fn new(activity: &proto::Activity) -> Kept {
        let value: serde_json::Value = serde_json::from_str(&activity.record).unwrap_or_default();
        Kept {
            activity: activity.clone(),
            mail_id: value["mail_id"].as_i64(),
            mail_ids: value["mail_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_i64)
                .collect(),
        }
    }
}

impl Stream {
    pub fn new(agent: &str) -> Stream {
        Stream {
            id: next_id(),
            agent: agent.to_string(),
            kinds: Kinds::All,
            records: VecDeque::new(),
            renderer: Activity::new(true).local_time().without_turns(),
            loaded: false,
            error: None,
            progress: ProgressState::default(),
            drawn: HashSet::new(),
            redraw: false,
        }
    }

    /// Stream everything the agent has shared, from its oldest kept event.
    pub fn watch(&self) -> Request {
        Request::WatchActivity {
            id: self.id,
            req: proto::ActivityRequest {
                backlog: BACKLOG,
                after_id: Some(0),
                agent: Some(self.agent.clone()),
            },
        }
    }

    pub fn unwatch(&self) -> Request {
        Request::Unwatch(self.id)
    }

    /// Start over, as a reload does.
    pub fn reset(&mut self) {
        *self = Stream {
            kinds: self.kinds,
            ..Stream::new(&self.agent)
        };
    }

    pub fn progress(&self, now: Instant) -> Progress {
        self.progress.at(now)
    }

    /// When the progress frame next changes, while it is fresh.
    pub fn next_frame_at(&self, now: Instant) -> Option<Instant> {
        self.progress.next_frame_at(now)
    }

    /// The newest record received.
    pub fn newest(&self) -> Option<i64> {
        self.records.back().map(|r| r.activity.id)
    }

    /// Tool calls still running, to show below the newest activity, each
    /// on one line of `width`.
    pub fn running(&self, width: u16) -> Vec<Line<'static>> {
        self.renderer
            .running_calls()
            .iter()
            .flat_map(|call| call.lines(Some(usize::from(width))))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether a kept record names any of `ids`, so loading or evicting
    /// them redraws it.
    pub fn references(&self, ids: &[i64]) -> bool {
        self.records.iter().any(|r| {
            r.mail_id.is_some_and(|id| ids.contains(&id))
                || r.mail_ids.iter().any(|id| ids.contains(id))
        })
    }

    /// Whether evicted records need the timeline redrawn from what is kept.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Draw `record` as an entry, if the kinds shown include it and the
    /// thread does not already show it as mail. A `text` record gives way to
    /// the automatic reply it was delivered as while that is loaded, and
    /// draws once per delivered-as id otherwise; a receipt or turn echoing
    /// mail gives way once all of it is loaded, a turn keeping its
    /// separator. Records without these ids, as older runners write, draw.
    fn entry(&mut self, kept: &Kept, loaded: &Loaded) -> Option<Entry> {
        let record = &kept.activity;
        let mut value: serde_json::Value = serde_json::from_str(&record.record).unwrap_or_default();
        let event = value["event"].as_str().unwrap_or_default().to_string();
        if !self.kinds.shows(&event) {
            return None;
        }
        let echoed =
            !kept.mail_ids.is_empty() && kept.mail_ids.iter().all(|id| loaded.contains_key(id));
        let lines = match (event.as_str(), kept.mail_id) {
            ("text", Some(id)) if loaded.get(&id) == Some(&true) || !self.drawn.insert(id) => {
                return None;
            }
            ("mail" | "steer", _) if echoed => return None,
            ("prompt", _) if echoed => {
                value["text"] = serde_json::Value::Null;
                self.renderer.render(&value.to_string())
            }
            _ => self.renderer.render(&record.record),
        };
        (!lines.is_empty()).then(|| Entry::Stream {
            id: record.id,
            created_at: record.created_at.clone(),
            lines,
            call: self.renderer.take_tool(),
        })
    }

    /// Every record kept, drawn afresh, as after changing the kinds or
    /// loading or evicting mail that records name.
    pub fn entries(&mut self, loaded: &Loaded) -> Vec<Entry> {
        self.renderer = Activity::new(true).local_time().without_turns();
        self.drawn.clear();
        let records = std::mem::take(&mut self.records);
        let entries = records
            .iter()
            .filter_map(|r| self.entry(r, loaded))
            .collect();
        self.records = records;
        entries
    }

    /// Take a batch of this stream's data at `now`, given the thread's
    /// loaded message ids; returns the new entries.
    pub fn on_data(&mut self, data: &Data, now: Instant, loaded: &Loaded) -> Vec<Entry> {
        if let Data::ActivityBaseline {
            id,
            through_id,
            record,
        } = data
            && *id == self.id
        {
            self.progress.baseline(*through_id, record.as_deref());
        }
        let Data::Activity { id, result } = data else {
            return Vec::new();
        };
        if *id != self.id {
            return Vec::new();
        }
        match result {
            Ok(events) => {
                for event in events {
                    self.progress.observe(event.id, &event.record, now);
                }
                self.loaded = true;
                self.error = None;
                let kept: Vec<_> = events.iter().map(Kept::new).collect();
                let entries = kept.iter().filter_map(|k| self.entry(k, loaded)).collect();
                self.records.extend(kept);
                let over = self.records.len().saturating_sub(MAX_RECORDS);
                // An evicted copy's row would outlive it in the larger view;
                // redraw from what is kept instead.
                if self.records.drain(..over).any(|r| r.mail_id.is_some()) {
                    self.drawn.clear();
                    self.redraw = true;
                }
                entries
            }
            Err(e) if e.contains(AGENT_ACTIVITY) => {
                self.progress.disconnect();
                self.error = Some(format!(
                    "Activity needs a newer server ({AGENT_ACTIVITY}). Ask its operator to upgrade."
                ));
                Vec::new()
            }
            Err(e) => {
                self.progress.disconnect();
                self.error = Some(e.clone());
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::thread::ThreadView;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::Modifier,
        widgets::{Block, Paragraph, Wrap},
    };

    #[test]
    fn tool_runs_start_after_the_top_or_a_non_tool_row_without_a_trailing_blank() {
        let stream = |tool: bool, lines: Vec<Line<'static>>| Entry::Stream {
            id: 1,
            created_at: "2026-10-16T18:00:00Z".into(),
            lines,
            call: tool.then(|| ToolLine::running("a")),
        };
        assert!(starts_tool_run(None), "the first run takes a blank too");
        assert!(!starts_tool_run(Some(&stream(
            true,
            vec![Line::raw("✓ a")]
        ))));
        assert!(starts_tool_run(Some(&stream(
            false,
            vec![Line::raw("text")]
        ))));
        // A row already ending with a blank line lends it to the run.
        let blank_after = stream(false, vec![Line::raw("text"), Line::default()]);
        assert!(!starts_tool_run(Some(&blank_after)));
        // Whitespace-only is visibly blank too, though it has width.
        let spaces = stream(false, vec![Line::raw("text"), Line::raw("   ")]);
        assert!(!starts_tool_run(Some(&spaces)));
        let tool = stream(true, vec![Line::raw("✓ b")]);
        assert!(tool.spaced_after(None) && !tool.spaced_after(Some(&tool.clone())));
        assert!(!stream(false, vec![Line::raw("text")]).spaced_after(None));
    }

    #[test]
    fn retried_copies_draw_once_and_eviction_redraws_from_what_is_kept() {
        let now = Instant::now();
        let mut stream = Stream::new("Mateo");
        let id = stream.id;
        let record = |n: i64, mail_id: Option<i64>| proto::Activity {
            id: n,
            created_at: "2026-10-16T18:00:00Z".into(),
            agent: "Mateo".into(),
            owner: None,
            record: match mail_id {
                Some(m) => format!(r#"{{"event":"text","text":"copy","mail_id":{m}}}"#),
                None => r#"{"event":"turn_end","stop_reason":"end_turn"}"#.into(),
            },
        };
        let push = |stream: &mut Stream, records: Vec<proto::Activity>| {
            let data = Data::Activity {
                id,
                result: Ok(records),
            };
            stream.on_data(&data, now, &Loaded::new())
        };
        let texts = |entries: &[Entry]| {
            entries
                .iter()
                .filter(|e| {
                    matches!(e, Entry::Stream { lines, .. }
                    if lines.iter().any(|l| l.to_string().contains("copy")))
                })
                .count()
        };
        // A retried copy of mail 1 does not draw again; mail 2 is its own.
        assert_eq!(texts(&push(&mut stream, vec![record(1, Some(1))])), 1);
        assert_eq!(texts(&push(&mut stream, vec![record(2, Some(1))])), 0);
        assert_eq!(texts(&push(&mut stream, vec![record(3, Some(2))])), 1);
        // Evicting the first copy asks for a redraw from what is kept,
        // which draws the retained copy once.
        let filler: Vec<_> = (4..=MAX_RECORDS as i64 + 1)
            .map(|n| record(n, None))
            .collect();
        push(&mut stream, filler);
        assert!(stream.take_redraw() && stream.drawn.is_empty());
        assert_eq!(texts(&stream.entries(&Loaded::new())), 2);
        assert_eq!(stream.drawn, HashSet::from([1, 2]));
        // Evicting the other records naming mail redraws to none; evicting
        // records that name no mail needs no redraw.
        let n = MAX_RECORDS as i64;
        push(&mut stream, vec![record(n + 2, None), record(n + 3, None)]);
        assert!(stream.take_redraw());
        assert_eq!(texts(&stream.entries(&Loaded::new())), 0);
        assert!(stream.drawn.is_empty());
        push(&mut stream, vec![record(n + 4, None)]);
        assert!(!stream.take_redraw());
    }

    #[test]
    fn progress_tracks_only_new_current_subscription_events_regardless_of_filters() {
        use std::time::Duration;
        let now = Instant::now();
        let mut stream = Stream::new("Mateo");
        let id = stream.id;
        let batch = |id, event_id, event: &str| Data::Activity {
            id,
            result: Ok(vec![proto::Activity {
                id: event_id,
                record: format!(r#"{{"event":"{event}","text":"hidden"}}"#),
                ..Default::default()
            }]),
        };
        stream.on_data(
            &Data::ActivityBaseline {
                id,
                through_id: 500,
                record: None,
            },
            now,
            &Loaded::new(),
        );
        stream.on_data(&batch(id, 500, "text"), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Unknown);
        stream.kinds = Kinds::Problems;
        assert!(
            stream
                .on_data(&batch(id, 501, "text"), now, &Loaded::new())
                .is_empty()
        );
        assert_eq!(stream.progress(now), Progress::Progressing(0));
        stream.entries(&Loaded::new());
        stream.on_data(
            &batch(id, 501, "text"),
            now + Duration::from_secs(59),
            &Loaded::new(),
        );
        assert_eq!(
            stream.progress(now + Duration::from_secs(60)),
            Progress::Unknown
        );
        // Recorded completion persists instead of expiring.
        stream.on_data(&batch(id, 502, "turn_end"), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Done);
        assert_eq!(
            stream.progress(now + Duration::from_secs(60)),
            Progress::Done
        );
        stream.reset();
        assert_ne!(stream.id, id);
        stream.on_data(
            &Data::ActivityBaseline {
                id,
                through_id: 0,
                record: None,
            },
            now,
            &Loaded::new(),
        );
        stream.on_data(&batch(id, 503, "text"), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Unknown);
        let id = stream.id;
        stream.on_data(
            &Data::ActivityBaseline {
                id,
                through_id: 503,
                record: None,
            },
            now,
            &Loaded::new(),
        );
        stream.on_data(&batch(id, 504, "text"), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Progressing(0));
        stream.on_data(
            &Data::Activity {
                id,
                result: Err("disconnected".into()),
            },
            now,
            &Loaded::new(),
        );
        assert_eq!(stream.progress(now), Progress::Unknown);
    }

    #[test]
    fn outgoing_background_covers_padding_wrapping_and_scrolled_rows() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for (scheme, compact, width) in SCHEMES.into_iter().flat_map(|scheme| {
            [false, true]
                .into_iter()
                .flat_map(move |compact| [24, 48].map(|width| (scheme, compact, width)))
        }) {
            with_scheme(scheme, || {
                let human = theme::base().patch(theme::style(theme::Role::HumanSurface));
                let make = |id, outgoing, body: &str| Entry::Message {
                    recipient: None,
                    message: Message {
                        id,
                        created_at: "2026-10-16T18:00:00Z".into(),
                        from_name: if outgoing { "ada" } else { "Mateo" }.into(),
                        from_kind: if outgoing {
                            proto::Kind::Human
                        } else {
                            proto::Kind::Agent
                        }
                        .into(),
                        to_name: Some(if outgoing { "Mateo" } else { "ada" }.into()),
                        body: body.into(),
                        ..Default::default()
                    },
                    outgoing,
                    headerless: compact,
                    gap: false,
                };
                let body = "**needle** followed by enough words to wrap across several full rendered rows.\n\n`code` and [link](https://example.com)";
                let outgoing = make(2, true, body);
                let incoming = make(2, false, body);
                let incoming_height = Paragraph::new(incoming.lines(false, None))
                    .wrap(Wrap { trim: false })
                    .line_count(width);
                let raw_lines = outgoing.lines(false, None).len();
                let mut view = ThreadView::new(
                    vec![make(1, false, "before"), outgoing, make(3, false, "after")],
                    200,
                );
                view.set_query("needle");
                let mut terminal = Terminal::new(TestBackend::new(width, 60)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(Block::new().style(theme::base()), frame.area());
                        view.render(frame, frame.area());
                    })
                    .unwrap();
                let start = view.frame.starts[1] as u16;
                let end = view.frame.starts[2] as u16;
                assert!(usize::from(end - start) > raw_lines, "body must wrap");
                if compact {
                    assert_eq!(
                        usize::from(end - start),
                        incoming_height + 2,
                        "compact outgoing adds one light row above and below the body"
                    );
                }
                let buffer = terminal.backend().buffer();
                for y in 0..60 {
                    for x in 0..width {
                        let cell = &buffer[(x, y)];
                        assert_eq!(
                            cell.bg,
                            if (start..end).contains(&y) && !(compact && y == start) {
                                human.bg.unwrap()
                            } else {
                                theme::base().bg.unwrap()
                            },
                            "at {x},{y}, compact={compact}"
                        );
                        if (start..end).contains(&y)
                            && cell.symbol() != " "
                            && theme::base().fg != theme::base().bg
                        {
                            assert_ne!(cell.fg, human.bg.unwrap(), "text must remain readable");
                        }
                    }
                }
                for y in [start, end - 1] {
                    assert!((0..width).all(|x| buffer[(x, y)].symbol() == " "));
                }
                if compact {
                    assert!((0..width).all(|x| buffer[(x, start + 1)].symbol() == " "));
                    let first_body: String = (0..width)
                        .map(|x| buffer[(x, start + 2)].symbol())
                        .collect();
                    assert!(first_body.contains("needle"));
                }
                if !compact {
                    assert_eq!(
                        buffer[(0, start + 1)].fg,
                        theme::style(theme::Role::Muted)
                            .fg
                            .or(theme::base().fg)
                            .unwrap()
                    );
                }
                assert!(buffer.content.iter().any(|c| c.symbol() == "n"
                    && c.modifier.contains(Modifier::BOLD | Modifier::REVERSED)));
                // The same full-row background survives a viewport clipped into the message.
                terminal.backend_mut().resize(width, 3);
                terminal
                    .resize(ratatui::layout::Rect::new(0, 0, width, 3))
                    .unwrap();
                view.layout(width, 3);
                view.scroll_to(usize::from(start));
                terminal
                    .draw(|frame| {
                        frame.render_widget(Block::new().style(theme::base()), frame.area());
                        view.render(frame, frame.area());
                    })
                    .unwrap();
                assert!((0..width).all(|x| terminal.backend().buffer()[(x, 0)].bg
                    == if compact {
                        theme::base().bg.unwrap()
                    } else {
                        human.bg.unwrap()
                    }));
                if compact {
                    view.scroll_to(usize::from(start) + 1);
                    terminal
                        .draw(|frame| {
                            frame.render_widget(Block::new().style(theme::base()), frame.area());
                            view.render(frame, frame.area());
                        })
                        .unwrap();
                    assert!((0..width).all(|x| {
                        let cell = &terminal.backend().buffer()[(x, 0)];
                        cell.symbol() == " " && cell.bg == human.bg.unwrap()
                    }));
                }
                view.scroll_to(usize::from(start) + 2);
                terminal
                    .draw(|frame| {
                        frame.render_widget(Block::new().style(theme::base()), frame.area());
                        view.render(frame, frame.area());
                    })
                    .unwrap();
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .all(|c| c.bg == human.bg.unwrap())
                );
            });
        }
    }
}
