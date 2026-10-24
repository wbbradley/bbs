//! A thread's timeline: messages and, in an agent's thread, the activity it
//! shares, drawn as `bbs headless logs --pretty` draws it, in time order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use chrono::DateTime;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::app::next_id;
use super::event::{Data, Request};
use super::progress::{Progress, ProgressState};
use super::theme;
use crate::backend::AGENT_ACTIVITY;
use crate::proto::{self, Message};
use crate::ui::activity::{Activity, ToolLine};
use crate::ui::message::{
    compact_message_lines, conversation_roles, find_matches, message_lines_with_recipient_at,
    message_prefix, side_header, side_message_lines,
};
use crate::ui::thread::{Row, TurnMarks};

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
        /// Side conversations and group mail, only in an agent conversation.
        other_participants: bool,
        /// A possible gap in the raw feed before this visible message.
        gap: bool,
    },
    /// One activity record, drawn when it arrived.
    Stream {
        id: i64,
        created_at: String,
        lines: Vec<Line<'static>>,
        /// Original reply/reasoning content for reflow at the live pane width.
        markdown: Option<String>,
        /// A finished tool call, kept whole and drawn as one line fitted to
        /// the width; runs of these sit together after one blank.
        call: Option<ToolLine>,
        /// Reasoning rows form tight groups with a blank line above.
        thought: bool,
        /// Assistant reply text uses the same inset as its delivered mail.
        reply: bool,
        /// The mail this record stands for: what a text was delivered as
        /// (`mail_id`) and what a receipt or turn echoes (`mail_ids`).
        echoes: Vec<i64>,
    },
}

impl Entry {
    /// The message a row shows, if it is mail.
    pub fn message_id(&self) -> Option<i64> {
        match self {
            Entry::Message { message, .. } => Some(message.id),
            Entry::Stream { .. } => None,
        }
    }

    /// What compact view hides: finished tool calls, mail among other
    /// participants, and any activity standing for such mail (`side`), even
    /// alongside direct mail; and reasoning when `thoughts` is set.
    pub fn compact_hides(&self, side: &HashSet<i64>, thoughts: bool) -> bool {
        match self {
            Entry::Message {
                other_participants, ..
            } => *other_participants,
            Entry::Stream {
                call,
                echoes,
                thought,
                ..
            } => {
                call.is_some()
                    || (thoughts && *thought)
                    || echoes.iter().any(|id| side.contains(id))
            }
        }
    }

    fn is_tool(&self) -> bool {
        matches!(self, Entry::Stream { call: Some(_), .. })
    }

    fn is_thought(&self) -> bool {
        matches!(self, Entry::Stream { thought: true, .. })
    }
}

impl Entry {
    fn rendered_lines(&self, gap: bool, query: Option<&str>, width: Option<u16>) -> Vec<Line<'_>> {
        match self {
            Entry::Message {
                message,
                headerless,
                recipient,
                outgoing,
                other_participants,
                gap: raw_gap,
            } => {
                let mut lines = if *headerless {
                    compact_message_lines(message, gap || *raw_gap, query, width)
                } else if *other_participants {
                    side_message_lines(message, gap || *raw_gap, query, *recipient, width)
                } else {
                    message_lines_with_recipient_at(
                        message,
                        gap || *raw_gap,
                        query,
                        *recipient,
                        width,
                    )
                };
                if *outgoing {
                    if *headerless {
                        lines.insert(1 + usize::from(gap || *raw_gap), Line::default());
                    }
                    // Until the agent reads it, the bottom padding says so,
                    // inset from the right as the body is from the left.
                    lines.push(if *headerless && message.read_at.is_none() {
                        Line::styled("unread  ", theme::style(theme::Role::HumanNote))
                            .right_aligned()
                    } else {
                        Line::default()
                    });
                    for line in &mut lines {
                        line.style = theme::style(theme::Role::HumanSurface).patch(line.style);
                    }
                }
                lines
            }
            Entry::Stream {
                markdown: Some(text),
                thought,
                ..
            } if width.is_some() => crate::ui::activity::markdown_lines(text, width, *thought),
            Entry::Stream { lines, .. } => lines.clone(),
        }
    }
}

/// Whether a line shows nothing: no text, or only whitespace.
fn blank(line: &Line) -> bool {
    line.spans
        .iter()
        .all(|s| s.content.chars().all(char::is_whitespace))
}

fn ends_blank(e: &Entry) -> bool {
    e.lines(false, None).last().is_some_and(blank)
}

fn starts_blank(e: &Entry) -> bool {
    e.lines(false, None).first().is_some_and(blank)
}

/// Whether one blank line goes between `prev` and `next` (none: the top or
/// the bottom): tool runs get both boundaries, reasoning groups only the
/// opening boundary. Reuse existing blanks and keep each group's rows tight.
pub fn needs_gap(prev: Option<&Entry>, next: Option<&Entry>) -> bool {
    let tool = |e: Option<&Entry>| e.is_some_and(Entry::is_tool);
    let opens_thought = next.is_some_and(Entry::is_thought) && !prev.is_some_and(Entry::is_thought);
    (tool(prev) != tool(next) || opens_thought)
        && !prev.is_some_and(ends_blank)
        && !next.is_some_and(starts_blank)
}

/// Whether tool calls drawn after `prev` (none: the top) open a run, and so
/// need a blank line above them.
pub fn opens_run(prev: Option<&Entry>) -> bool {
    !prev.is_some_and(|p| p.is_tool() || ends_blank(p))
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
        self.rendered_lines(gap, query, None)
    }

    fn style(&self) -> Style {
        match self {
            Entry::Message { outgoing: true, .. } => theme::style(theme::Role::HumanSurface),
            Entry::Message {
                other_participants: true,
                message,
                ..
            } => theme::style(conversation_roles(message).0),
            _ => Style::default(),
        }
    }

    fn inset(&self) -> u16 {
        match self {
            Entry::Message {
                headerless: true, ..
            }
            | Entry::Message {
                other_participants: true,
                ..
            }
            | Entry::Stream { reply: true, .. }
            | Entry::Stream { thought: true, .. } => 2,
            _ => 0,
        }
    }

    fn gutter(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        if self.is_thought() {
            let height = Paragraph::new(self.lines_at(gap, query, width))
                .wrap(Wrap { trim: false })
                .line_count(width);
            return vec![Line::styled("│ ", theme::dim().italic()); height];
        }
        let Entry::Message {
            other_participants: true,
            message,
            recipient,
            gap: raw_gap,
            ..
        } = self
        else {
            return Vec::new();
        };
        let measure = |lines: Vec<Line>| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .line_count(width)
        };
        let prefix_height = measure(message_prefix(gap || *raw_gap));
        let header_height = measure(side_header(message, query, *recipient, Some(width)));
        let surface = theme::style(conversation_roles(message).1);
        let mut gutter = vec![Line::default(); prefix_height];
        for index in 0..header_height {
            gutter.push(
                Line::from(Span::styled(
                    if index == 0 { "• " } else { "  " },
                    surface.patch(theme::style(if message.to_group.is_some() {
                        theme::Role::Group
                    } else {
                        theme::Role::Accent
                    })),
                ))
                .style(surface),
            );
        }
        gutter
    }

    fn separator_style(&self) -> Option<Style> {
        matches!(
            self,
            Entry::Message {
                other_participants: true,
                ..
            } | Entry::Message {
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
            } => vec![call.one_line(usize::from(width))],
            _ => self.rendered_lines(gap, query, Some(width)),
        }
    }

    /// One blank above reasoning groups, and above and below tool runs;
    /// reuse existing blanks and keep consecutive rows in each group tight.
    fn spaced_after(&self, prev: Option<&Self>) -> bool {
        needs_gap(prev, Some(self))
    }

    fn spaced_last(&self) -> bool {
        needs_gap(Some(self), None)
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
    /// Startup replay ends at the newest id sampled before subscribing.
    history_end: Option<i64>,
    history_ready: bool,
    /// The stream's last error, cleared by the next successful batch.
    pub error: Option<String>,
    /// Older-history failure, retained until the conversation is reloaded.
    pub history_error: Option<String>,
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
    boundary: Option<Boundary>,
}

#[derive(Clone, Copy)]
enum Boundary {
    Start,
    End,
    Reset,
}

impl Kept {
    fn new(activity: &proto::Activity) -> Kept {
        let value: serde_json::Value = serde_json::from_str(&activity.record).unwrap_or_default();
        Kept {
            activity: activity.clone(),
            mail_id: value["mail_id"].as_i64(),
            boundary: match value["event"].as_str() {
                Some("prompt") => Some(Boundary::Start),
                Some("turn_end" | "turn_error" | "cancel" | "stop") => Some(Boundary::End),
                Some("start" | "session") => Some(Boundary::Reset),
                _ => None,
            },
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
    /// Project recorded turn boundaries onto the content the current filters
    /// actually show. Echoed prompts can be displayed as their original mail;
    /// delivered replies can likewise replace activity without losing a mark.
    pub fn turn_marks(&self, rows: &[Entry]) -> HashMap<<Entry as Row>::Key, TurnMarks> {
        let mut marks = HashMap::<_, TurnMarks>::new();
        let messages: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| row.message_id().map(|id| (id, i)))
            .collect();
        let mut start: Option<&Kept> = None;
        let mut partial = true;
        let mut mark = |start: &Kept, end: Option<&Kept>, began: bool, ended: bool| {
            let low = (micros(&start.activity.created_at), 1, start.activity.id);
            let high = end.map(|end| (micros(&end.activity.created_at), 1, end.activity.id));
            let from = rows.partition_point(|row| row.key() < low);
            let from = start
                .mail_ids
                .iter()
                .filter_map(|id| messages.get(id))
                .copied()
                .min()
                .map_or(from, |echoed| from.min(echoed));
            let until = high.map_or(rows.len(), |high| {
                rows.partition_point(|row| row.key() < high || (ended && row.key() == high))
            });
            if from >= until {
                return;
            }
            let mut candidates = rows[from..until].iter().filter(|row| {
                (row.key() >= low
                    || row
                        .message_id()
                        .is_some_and(|id| start.mail_ids.contains(&id)))
                    && !row.lines(false, None).iter().all(blank)
            });
            if let Some(first) = candidates.next() {
                if began {
                    marks.entry(first.key()).or_default().start = true;
                }
                if ended {
                    let last = candidates.next_back().unwrap_or(first);
                    marks.entry(last.key()).or_default().end = true;
                }
            }
        };
        for kept in &self.records {
            match kept.boundary {
                Some(Boundary::Start) => {
                    if let Some(previous) = start.replace(kept) {
                        // A missing end never invents a stop marker.
                        mark(previous, Some(kept), true, false);
                    }
                }
                Some(Boundary::End) => {
                    if let Some(start) = start.take() {
                        mark(start, Some(kept), true, true);
                    } else if partial && let Some(first) = self.records.front() {
                        // The retained window may begin halfway through a turn.
                        mark(first, Some(kept), false, true);
                    }
                }
                Some(Boundary::Reset) => {
                    if let Some(start) = start.take() {
                        mark(start, Some(kept), true, false);
                    }
                }
                None => {}
            }
            if kept.boundary.is_some() {
                partial = false;
            }
        }
        if let Some(start) = start {
            mark(start, None, true, false);
        }
        marks
    }

    pub fn new(agent: &str) -> Stream {
        Stream {
            id: next_id(),
            agent: agent.to_string(),
            kinds: Kinds::All,
            records: VecDeque::new(),
            renderer: Activity::new(true)
                .local_time()
                .without_turns()
                .compact_thoughts(),
            loaded: false,
            history_end: None,
            history_ready: false,
            error: None,
            history_error: None,
            progress: ProgressState::default(),
            drawn: HashSet::new(),
            redraw: false,
        }
    }

    /// Stream everything the agent has shared: its newest records first, so
    /// the conversation's end shows at once, then the older ones it keeps.
    pub fn watch(&self) -> Request {
        Request::WatchActivity {
            id: self.id,
            req: proto::ActivityRequest {
                backlog: BACKLOG,
                after_id: None,
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

    pub fn history_ready(&self) -> bool {
        self.history_ready || self.error.is_some()
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
            .map(|call| call.one_line(usize::from(width)))
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
            markdown: if matches!(event.as_str(), "text" | "thought") {
                value["text"].as_str().map(str::to_owned)
            } else {
                None
            },
            call: self.renderer.take_tool(),
            thought: event == "thought",
            reply: event == "text",
            echoes: kept.mail_ids.iter().copied().chain(kept.mail_id).collect(),
        })
    }

    /// Every record kept, drawn afresh, as after changing the kinds or
    /// loading or evicting mail that records name.
    pub fn entries(&mut self, loaded: &Loaded) -> Vec<Entry> {
        self.renderer = Activity::new(true)
            .local_time()
            .without_turns()
            .compact_thoughts();
        self.drawn.clear();
        let records = std::mem::take(&mut self.records);
        let entries = records
            .iter()
            .filter_map(|r| self.entry(r, loaded))
            .collect();
        self.records = records;
        entries
    }

    /// Put retained history older than every kept record in front, newest
    /// kept first under the cap, and have the thread redraw once: drawing
    /// it all in order rebuilds the renderer's state, so no old call shows
    /// as running and progress, which history never moves, stays put.
    fn backfill(&mut self, older: &[proto::Activity]) {
        let oldest = self.records.front().map(|r| r.activity.id);
        let older: Vec<Kept> = older
            .iter()
            .filter(|row| oldest.is_none_or(|oldest| row.id < oldest))
            .map(Kept::new)
            .collect();
        let room = MAX_RECORDS.saturating_sub(self.records.len());
        let skip = older.len().saturating_sub(room);
        if older.len() == skip {
            return;
        }
        for kept in older.into_iter().skip(skip).rev() {
            self.records.push_front(kept);
        }
        self.drawn.clear();
        self.redraw = true;
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
            self.history_end = Some(*through_id);
            self.progress = ProgressState::default();
            self.progress.baseline(*through_id, record.as_deref());
        }
        if let Data::ActivityBackfill { id, result } = data
            && *id == self.id
        {
            match result {
                Ok(older) => self.backfill(older),
                Err(e) => self.history_error = Some(e.clone()),
            }
            return Vec::new();
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
                self.history_ready |= events.len() < BACKLOG as usize
                    || events
                        .last()
                        .is_some_and(|row| row.id >= self.history_end.unwrap_or(0));
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
    fn turn_marks_follow_visible_replacements_and_do_not_cross_restarts() {
        let mut stream = Stream::new("Mateo");
        let record = |id, event: serde_json::Value| proto::Activity {
            id,
            created_at: format!("2026-10-24T12:00:{id:02}Z"),
            record: event.to_string(),
            ..Default::default()
        };
        let records = vec![
            record(1, serde_json::json!({"event":"prompt","text":""})),
            record(2, serde_json::json!({"event":"thought","text":"thinking"})),
            record(
                3,
                serde_json::json!({"event":"text","text":"answer","mail_id":42}),
            ),
            record(4, serde_json::json!({"event":"turn_end"})),
            record(5, serde_json::json!({"event":"text","text":"outside"})),
            record(6, serde_json::json!({"event":"prompt","text":""})),
            record(
                7,
                serde_json::json!({"event":"turn_error","message":"failed"}),
            ),
            record(8, serde_json::json!({"event":"prompt","text":""})),
            record(9, serde_json::json!({"event":"text","text":"unfinished"})),
            record(10, serde_json::json!({"event":"start"})),
            record(11, serde_json::json!({"event":"text","text":"new session"})),
        ];
        stream.on_data(
            &Data::Activity {
                id: stream.id,
                result: Ok(records),
            },
            Instant::now(),
            &Loaded::new(),
        );
        let rows = stream.entries(&Loaded::new());
        let key = |id| (micros(&format!("2026-10-24T12:00:{id:02}Z")), 1, id);
        let marks = stream.turn_marks(&rows);
        assert_eq!(
            marks.get(&key(2)),
            Some(&TurnMarks {
                start: true,
                end: false
            })
        );
        assert_eq!(
            marks.get(&key(3)),
            Some(&TurnMarks {
                start: false,
                end: true
            })
        );
        assert_eq!(
            marks.get(&key(7)),
            Some(&TurnMarks {
                start: true,
                end: true
            })
        );
        assert_eq!(
            marks.get(&key(9)),
            Some(&TurnMarks {
                start: true,
                end: false
            })
        );
        assert_eq!(marks.len(), 4);

        let mut rows = stream.entries(&Loaded::from([(42, true)]));
        rows.retain(|row| !row.is_thought());
        let delivered = Entry::Message {
            message: Message {
                id: 42,
                created_at: "2026-10-24T12:00:03Z".into(),
                from_name: "Mateo".into(),
                from_kind: proto::Kind::Agent.into(),
                body: "answer".into(),
                ..Default::default()
            },
            headerless: true,
            recipient: None,
            outgoing: false,
            other_participants: false,
            gap: false,
        };
        let delivered_key = delivered.key();
        rows.push(delivered);
        rows.sort_by_key(Row::key);
        let marks = stream.turn_marks(&rows);
        assert_eq!(
            marks.get(&delivered_key),
            Some(&TurnMarks {
                start: true,
                end: true
            })
        );
        assert_eq!(marks.len(), 3);
        // A retained window beginning inside the first turn keeps its known
        // end, without inventing a beginning on the first retained line.
        stream.records.pop_front();
        let marks = stream.turn_marks(&rows);
        assert_eq!(
            marks.get(&delivered_key),
            Some(&TurnMarks {
                start: false,
                end: true
            })
        );
    }

    #[test]
    fn older_history_merges_once_in_order_without_reviving_tools_or_progress() {
        let now = Instant::now();
        let row = |id: i64, record: &str| proto::Activity {
            id,
            record: record.into(),
            ..Default::default()
        };
        let mut stream = Stream::new("Mateo");
        let tail_record = r#"{"event":"tool","id":"t2","status":"pending","title":"cargo test"}"#;
        stream.on_data(
            &Data::ActivityBaseline {
                id: stream.id,
                through_id: 20,
                record: Some(tail_record.into()),
            },
            now,
            &Loaded::new(),
        );
        // The newest window: a turn begins at 11; a call is running at 20.
        let mut window = vec![row(11, r#"{"event":"prompt","text":"go"}"#)];
        window.extend((12..20).map(|id| row(id, r#"{"event":"thought","text":"newer"}"#)));
        window.push(row(20, tail_record));
        stream.on_data(
            &Data::Activity {
                id: stream.id,
                result: Ok(window),
            },
            now,
            &Loaded::new(),
        );
        assert!(stream.history_ready());
        assert_eq!(stream.running(80).len(), 1);
        let progress = stream.progress(now);
        assert!(!stream.take_redraw());
        // Older history: a call that never finished before that turn began,
        // and a copy of the window's first record.
        let older = vec![
            row(
                1,
                r#"{"event":"tool","id":"t1","status":"pending","title":"git log"}"#,
            ),
            row(2, r#"{"event":"text","text":"older text"}"#),
            row(11, r#"{"event":"prompt","text":"go"}"#),
        ];
        let entries = stream.on_data(
            &Data::ActivityBackfill {
                id: stream.id,
                result: Ok(older.clone()),
            },
            now,
            &Loaded::new(),
        );
        assert!(entries.is_empty(), "never drawn piecemeal");
        assert!(stream.take_redraw(), "the thread redraws once");
        let ids: Vec<i64> = stream.records.iter().map(|r| r.activity.id).collect();
        assert_eq!(ids, [1, 2].into_iter().chain(11..=20).collect::<Vec<_>>());
        let entries = stream.entries(&Loaded::new());
        let Entry::Stream { id, .. } = &entries[0] else {
            panic!("an activity entry");
        };
        assert_eq!(*id, 2, "the oldest drawn first");
        assert_eq!(
            stream.running(80).len(),
            1,
            "the old call ended with its turn; only the live one runs"
        );
        assert_eq!(
            stream.progress(now),
            progress,
            "history never moves progress"
        );
        // Another copy, or one for another stream, changes nothing.
        for id in [stream.id, stream.id + 1] {
            stream.on_data(
                &Data::ActivityBackfill {
                    id,
                    result: Ok(older.clone()),
                },
                now,
                &Loaded::new(),
            );
            assert!(!stream.take_redraw());
        }
        // The cap keeps the newest: history fills only the room left.
        let mut full = Stream::new("Mateo");
        full.on_data(
            &Data::Activity {
                id: full.id,
                result: Ok(vec![row(
                    MAX_RECORDS as i64 + 1,
                    r#"{"event":"thought","text":"x"}"#,
                )]),
            },
            now,
            &Loaded::new(),
        );
        let older: Vec<_> = (1..=MAX_RECORDS as i64)
            .map(|id| row(id, r#"{"event":"thought","text":"x"}"#))
            .collect();
        full.on_data(
            &Data::ActivityBackfill {
                id: full.id,
                result: Ok(older),
            },
            now,
            &Loaded::new(),
        );
        assert_eq!(full.records.len(), MAX_RECORDS);
        assert_eq!(full.records.front().unwrap().activity.id, 2);
        // A refused fetch says so, like a stream error.
        full.on_data(
            &Data::ActivityBackfill {
                id: full.id,
                result: Err("refused".into()),
            },
            now,
            &Loaded::new(),
        );
        assert_eq!(full.history_error.as_deref(), Some("refused"));
        full.on_data(
            &Data::Activity {
                id: full.id,
                result: Ok(vec![row(MAX_RECORDS as i64 + 2, r#"{"event":"turn_end"}"#)]),
            },
            now,
            &Loaded::new(),
        );
        assert!(full.error.is_none());
        assert_eq!(full.history_error.as_deref(), Some("refused"));
    }

    #[test]
    fn runner_restart_and_hidden_turn_end_clear_stale_running_tools() {
        let mut stream = Stream::new("Mateo");
        let now = Instant::now();
        let mut sequence = 0;
        let mut push = |stream: &mut Stream, record: &str| {
            sequence += 1;
            stream.on_data(
                &Data::Activity {
                    id: stream.id,
                    result: Ok(vec![proto::Activity {
                        id: sequence,
                        record: record.into(),
                        ..Default::default()
                    }]),
                },
                now,
                &Loaded::new(),
            )
        };
        push(
            &mut stream,
            r#"{"event":"tool","id":"old","status":"pending","title":"git show"}"#,
        );
        push(
            &mut stream,
            r#"{"event":"tool","id":"old","title":"gh api"}"#,
        );
        assert_eq!(stream.running(80).len(), 1);
        // A runner may disappear without ever sharing a stop or tool result.
        push(&mut stream, r#"{"event":"start"}"#);
        push(&mut stream, r#"{"event":"session","resumed":true}"#);
        assert!(stream.running(80).is_empty());
        stream.entries(&Loaded::new());
        assert!(stream.running(80).is_empty(), "history replay must agree");
        push(
            &mut stream,
            r#"{"event":"tool","id":"new","status":"pending","title":"cargo test"}"#,
        );
        assert_eq!(stream.running(80).len(), 1);
        assert!(push(&mut stream, r#"{"event":"turn_end"}"#).is_empty());
        assert!(
            stream.running(80).is_empty(),
            "hidden boundaries still retire tools"
        );
        stream.entries(&Loaded::new());
        assert!(stream.running(80).is_empty());
    }

    #[test]
    fn reconnected_activity_refreshes_progress_without_replaying_old_rows() {
        let mut stream = Stream::new("Mateo");
        let id = stream.id;
        let now = Instant::now();
        let baseline = |through_id| Data::ActivityBaseline {
            id,
            through_id,
            record: Some(r#"{"event":"turn_end"}"#.into()),
        };
        let data = |n| Data::Activity {
            id,
            result: Ok(vec![proto::Activity {
                id: n,
                record: r#"{"event":"thought","text":"working"}"#.into(),
                ..Default::default()
            }]),
        };
        stream.on_data(&baseline(1), now, &Loaded::new());
        stream.on_data(&data(2), now, &Loaded::new());
        assert!(matches!(stream.progress(now), Progress::Progressing(_)));
        stream.on_data(
            &Data::Activity {
                id,
                result: Err("reconnecting".into()),
            },
            now,
            &Loaded::new(),
        );
        assert_eq!(stream.progress(now), Progress::Unknown);
        stream.on_data(&baseline(3), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Done);
        // Catch-up is history; only events beyond the reconnect watermark animate.
        stream.on_data(&data(3), now, &Loaded::new());
        assert_eq!(stream.progress(now), Progress::Done);
        stream.on_data(&data(4), now, &Loaded::new());
        assert!(matches!(stream.progress(now), Progress::Progressing(_)));
        assert!(stream.error.is_none());
        assert_eq!(
            stream
                .records
                .iter()
                .map(|r| r.activity.id)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
    }

    #[test]
    fn initial_history_handles_empty_short_and_crossed_boundary_batches() {
        for (through, count, end, ready) in [
            (0, 0, 0, true),
            (500, 199, 499, true),
            (500, 200, 400, false),
            (500, 200, 501, true),
        ] {
            let mut stream = Stream::new("Mateo");
            let now = Instant::now();
            stream.on_data(
                &Data::ActivityBaseline {
                    id: stream.id,
                    through_id: through,
                    record: None,
                },
                now,
                &Loaded::new(),
            );
            // Another subscription must not release this one's history gate.
            stream.on_data(
                &Data::Activity {
                    id: stream.id + 1,
                    result: Ok(vec![]),
                },
                now,
                &Loaded::new(),
            );
            assert!(!stream.history_ready());
            let rows = (0..count)
                .map(|i| proto::Activity {
                    id: end - count + i + 1,
                    record: r#"{"event":"thought","text":"history"}"#.into(),
                    ..Default::default()
                })
                .collect();
            stream.on_data(
                &Data::Activity {
                    id: stream.id,
                    result: Ok(rows),
                },
                now,
                &Loaded::new(),
            );
            assert_eq!(stream.history_ready(), ready);
            stream.reset();
            assert!(!stream.history_ready());
        }
    }

    #[test]
    fn reasoning_renders_markdown_without_blank_lines_and_preserves_raw_logs() {
        let mut stream = Stream::new("Mateo");
        let source = serde_json::json!({"event":"thought", "text":"\n \n\t**Pushing changes upstream**\n\n**Clarifying GitHub installation**"}).to_string();
        let record = proto::Activity {
            id: 1,
            record: source.clone(),
            ..Default::default()
        };
        let kept = Kept::new(&record);
        let entry = stream.entry(&kept, &Loaded::new()).unwrap();
        let lines = entry.lines(false, None);
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["Pushing changes upstream", "Clarifying GitHub installation"]
        );
        assert!(lines.iter().all(|line| line.spans.iter().any(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        })));
        stream.records.push_back(Kept::new(&record));
        let rebuilt = stream.entries(&Loaded::new());
        assert_eq!(rebuilt[0].lines(false, None), lines);
        assert_eq!(kept.activity.record, source);
        let pretty = Activity::new(false).render(&source);
        assert_eq!(pretty[0].to_string(), "│", "CLI logs are unchanged");
        let blank = Kept::new(&proto::Activity {
            id: 2,
            record: serde_json::json!({"event":"thought", "text":" \n\t"}).to_string(),
            ..Default::default()
        });
        assert!(stream.entry(&blank, &Loaded::new()).is_none());
    }

    #[test]
    fn reasoning_gutter_repeats_on_wrapped_lines_after_resize_and_scroll() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        let text = format!(
            "**Heading** {}\n\n- first item\n  - nested item\n\n```python\nif ready:\n    run()\n```\n\n[docs](https://example.com) and `code`",
            "Unicode 界 café words ".repeat(16)
        );
        let source = serde_json::json!({"event":"thought", "text":text}).to_string();
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut stream = Stream::new("Mateo");
                let record = Kept::new(&proto::Activity {
                    id: 1,
                    record: source.clone(),
                    ..Default::default()
                });
                let entry = stream.entry(&record, &Loaded::new()).unwrap();
                assert!(!entry.compact_hides(&HashSet::new(), false));
                assert!(entry.compact_hides(&HashSet::new(), true));
                let mut view = ThreadView::new(vec![entry.clone()], 100);
                view.set_query("heading");
                assert_eq!(view.search_status(), Some(("heading", 1)));
                for width in [28, 48, 80, 32] {
                    view.follow();
                    let mut terminal = Terminal::new(TestBackend::new(width, 200)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buf = terminal.backend().buffer();
                    let lines: Vec<_> = (0..view.frame.total as u16)
                        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                        .collect();
                    assert!(lines[0].trim().is_empty(), "group separator has no gutter");
                    assert!(lines[1].starts_with("│ Heading"), "{lines:?}");
                    assert!(
                        lines[1..].iter().all(|line| line.starts_with("│ ")),
                        "{lines:?}"
                    );
                    assert!(
                        lines.iter().any(|line| line.trim_end() == "│     run()"),
                        "{lines:?}"
                    );
                    assert!(buf[(2, 1)].modifier.contains(Modifier::BOLD));
                    assert!(buf[(0, 1)].modifier.contains(Modifier::ITALIC));
                    crate::ui::theme::testing::assert_buffer(buf);
                    let expected = Paragraph::new(entry.lines(false, None))
                        .wrap(Wrap { trim: false })
                        .line_count(width - 2)
                        + 1;
                    assert_eq!(view.frame.total, expected);
                    assert!(expected > entry.lines(false, None).len() + 1);
                    terminal.backend_mut().resize(width, 5);
                    terminal
                        .resize(ratatui::layout::Rect::new(0, 0, width, 5))
                        .unwrap();
                    view.layout(width, 5);
                    view.scroll_to(3);
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    for y in 0..5 {
                        let shown = (0..width)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>();
                        assert_eq!(shown, lines[view.top() + y as usize]);
                        assert!(shown.starts_with("│ "));
                    }
                    assert_eq!(view.search_status(), Some(("heading", 1)));
                }
                assert_eq!(record.activity.record, source);
            });
        }
    }

    #[test]
    fn one_gap_on_each_side_of_a_tool_run_unless_a_blank_is_already_there() {
        let row = |tool: bool, lines: Vec<Line<'static>>| Entry::Stream {
            id: 1,
            created_at: "2026-10-16T18:00:00Z".into(),
            lines,
            markdown: None,
            call: tool.then(|| ToolLine::running("a")),
            thought: false,
            reply: false,
            echoes: Vec::new(),
        };
        let call = row(true, vec![Line::raw("✓ a")]);
        let text = row(false, vec![Line::raw("text")]);
        // Runs open and close against text and against the edges.
        assert!(needs_gap(None, Some(&call)), "a leading run");
        assert!(needs_gap(Some(&text), Some(&call)));
        assert!(needs_gap(Some(&call), Some(&text)));
        assert!(needs_gap(Some(&call), None), "a trailing run");
        // Never within a run, between non-tool rows, or at a non-tool edge.
        assert!(!needs_gap(Some(&call), Some(&call)));
        assert!(!needs_gap(Some(&text), Some(&text)));
        assert!(!needs_gap(None, Some(&text)) && !needs_gap(Some(&text), None));
        // A blank already at the boundary, empty or whitespace-only, serves.
        for blank in [Line::default(), Line::raw("   ")] {
            let ends = row(false, vec![Line::raw("text"), blank.clone()]);
            let starts = row(false, vec![blank, Line::raw("text")]);
            assert!(!needs_gap(Some(&ends), Some(&call)));
            assert!(!needs_gap(Some(&call), Some(&starts)));
            assert!(!opens_run(Some(&ends)));
        }
        assert!(opens_run(None) && opens_run(Some(&text)) && !opens_run(Some(&call)));
        assert!(call.spaced_after(None) && call.spaced_last());
        assert!(!call.spaced_after(Some(&call)) && text.spaced_after(Some(&call)));
    }

    #[test]
    fn reply_inset_survives_wrapping_resize_search_and_side_header_scroll() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let body = format!(
                    "**needle** {}\n\nSecond paragraph.\n\n- item\n\n```python\n    code()\n```",
                    "Unicode 界 café words ".repeat(12)
                );
                let message = Message {
                    id: 1,
                    from_name: "Theo".into(),
                    from_kind: proto::Kind::Agent.into(),
                    to_name: Some("Tim".into()),
                    body: body.clone(),
                    ..Default::default()
                };
                let entry = Entry::Message {
                    message: message.clone(),
                    headerless: true,
                    recipient: Some(proto::Kind::Agent),
                    outgoing: false,
                    other_participants: false,
                    gap: false,
                };
                let mut view = ThreadView::new(vec![entry], 100);
                view.set_query("needle");
                for width in [80, 32, 100] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 80)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buf = terminal.backend().buffer();
                    crate::ui::theme::testing::assert_buffer(buf);
                    let lines: Vec<_> = (0..view.frame.total as u16)
                        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                        .collect();
                    assert!(lines.iter().all(|line| line.starts_with("  ")), "{lines:?}");
                    assert!(
                        lines[1].starts_with("  needle"),
                        "no extra first-line inset"
                    );
                    assert!(
                        lines[2].starts_with("  ") && !lines[2][2..].starts_with(' '),
                        "wrapped continuation: {:?}",
                        lines[2]
                    );
                    assert!(
                        lines.iter().any(|line| line.trim_end() == "      code()"),
                        "code keeps its own indentation: {lines:?}"
                    );
                    assert!(buf.content.iter().any(|cell| cell.symbol() == "n"
                        && cell.modifier.contains(Modifier::BOLD | Modifier::REVERSED)));
                    assert_eq!(
                        view.frame.total,
                        Paragraph::new(view.rows[0].lines(false, None))
                            .wrap(Wrap { trim: false })
                            .line_count(width - 2)
                    );
                    view.scroll_to(2);
                    terminal.backend_mut().resize(width, 3);
                    terminal
                        .resize(ratatui::layout::Rect::new(0, 0, width, 3))
                        .unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    assert!((0..3).all(|y| {
                        (0..2).all(|x| terminal.backend().buffer()[(x, y)].symbol() == " ")
                    }));
                    view.follow();
                }
                // Undelivered/legacy text activity uses the identical wrapping margin.
                let mut stream = Stream::new("Theo");
                let record = proto::Activity {
                    id: 1,
                    record: serde_json::json!({"event":"text","text":body}).to_string(),
                    ..Default::default()
                };
                let text = stream.entry(&Kept::new(&record), &Loaded::new()).unwrap();
                assert_eq!(text.inset(), 2);
                // A wrapped side header keeps one bullet in column zero, names
                // at column two, and its tint through the gutter and scrolling.
                for group in [false, true] {
                    let header_role = if group {
                        theme::Role::GroupConversationHeader
                    } else {
                        theme::Role::OtherConversationHeader
                    };
                    let bullet_role = if group {
                        theme::Role::Group
                    } else {
                        theme::Role::Accent
                    };
                    let side = Entry::Message {
                        message: Message {
                            from_name: "Theo長い名前".repeat(4),
                            to_group: group.then(|| "long-group-name".repeat(3)),
                            ..message.clone()
                        },
                        headerless: false,
                        recipient: Some(proto::Kind::Agent),
                        outgoing: false,
                        other_participants: true,
                        gap: true,
                    };
                    let mut view = ThreadView::new(vec![side], 100);
                    let width = 32;
                    let mut terminal = Terminal::new(TestBackend::new(width, 80)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buf = terminal.backend().buffer();
                    let header = (0..view.frame.total as u16)
                        .find(|&y| buf[(0, y)].symbol() == "•")
                        .unwrap();
                    assert!(header > 2, "wrapped gap warning comes before header");
                    assert_eq!(buf[(2, header)].symbol(), "T");
                    assert_eq!(
                        buf[(0, header)].fg,
                        theme::base().patch(theme::style(bullet_role)).fg.unwrap()
                    );
                    assert_eq!(buf[(0, header + 1)].symbol(), " ");
                    let header_bg = theme::base().patch(theme::style(header_role)).bg.unwrap();
                    let header_rows = view.rows[0].gutter(true, None, width - 2).len();
                    for y in header..header_rows as u16 {
                        let mut x = 0;
                        while x < width {
                            assert_eq!(buf[(x, y)].bg, header_bg, "wrapped header at {x},{y}");
                            // TestBackend clears the hidden continuation cells of
                            // wide glyphs; the displayed glyph supplies their style.
                            x += unicode_width::UnicodeWidthStr::width(buf[(x, y)].symbol()).max(1)
                                as u16;
                        }
                    }
                    assert_eq!(
                        buf[(0, header + 1)].bg,
                        theme::base().patch(theme::style(header_role)).bg.unwrap()
                    );
                    assert_eq!(buf.content.iter().filter(|c| c.symbol() == "•").count(), 1);
                    view.layout(width, 3);
                    view.scroll_to(header as usize + 1);
                    terminal.backend_mut().resize(width, 3);
                    terminal
                        .resize(ratatui::layout::Rect::new(0, 0, width, 3))
                        .unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), " ");
                    let mut x = 0;
                    while x < width {
                        let cell = &terminal.backend().buffer()[(x, 0)];
                        assert_eq!(cell.bg, header_bg);
                        x += unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
                    }
                    assert_eq!(
                        terminal.backend().buffer()[(0, 0)].bg,
                        theme::base().patch(theme::style(header_role)).bg.unwrap()
                    );
                }
            });
        }
    }

    #[test]
    fn side_header_background_fills_the_row_without_recoloring_body_or_search() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        use ratatui::style::Modifier;
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let side = Entry::Message {
                    message: Message {
                        id: 1,
                        from_name: "Jefe".into(),
                        from_kind: proto::Kind::Agent.into(),
                        to_name: Some("John".into()),
                        body: "side body".into(),
                        ..Default::default()
                    },
                    headerless: false,
                    recipient: Some(proto::Kind::Agent),
                    outgoing: false,
                    other_participants: true,
                    gap: false,
                };
                let mut view = ThreadView::new(vec![side], 100);
                view.set_query("jefe");
                for width in [80, 120] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buf = terminal.backend().buffer();
                    for (y, role) in [
                        theme::Role::Screen,
                        theme::Role::OtherConversationHeader,
                        theme::Role::OtherConversation,
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let bg = theme::base().patch(theme::style(role)).bg.unwrap();
                        for x in 0..width {
                            assert_eq!(buf[(x, y as u16)].bg, bg, "{x},{y}");
                        }
                    }
                    assert_eq!(buf[(0, 1)].symbol(), "•");
                    assert_eq!(buf[(2, 1)].symbol(), "J");
                    assert!(buf[(2, 1)].modifier.contains(Modifier::REVERSED));
                    assert_eq!(
                        buf[(2, 1)].fg,
                        theme::base()
                            .patch(theme::style(theme::Role::Agent))
                            .fg
                            .unwrap()
                    );
                    assert_eq!(buf[(2, 2)].symbol(), "s");
                }
            });
        }
    }

    #[test]
    fn direct_side_cards_show_their_local_time_in_the_header_corner() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        let card = |id, from: &str, to: &str| Entry::Message {
            message: Message {
                id,
                created_at: "2026-10-17T03:30:00Z".into(),
                from_name: from.into(),
                from_kind: proto::Kind::Agent.into(),
                to_name: Some(to.into()),
                body: "side body".into(),
                ..Default::default()
            },
            headerless: false,
            recipient: Some(proto::Kind::Agent),
            outgoing: false,
            other_participants: true,
            gap: false,
        };
        let sent = chrono::DateTime::parse_from_rfc3339("2026-10-17T03:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %-I:%M:%S%P")
            .to_string();
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let long = "a-name-long-enough-to-crowd-the-time";
                let mut view =
                    ThreadView::new(vec![card(1, "Zed", "Mateo"), card(2, long, long)], 100);
                for width in [60u16, 80, 60] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
                    terminal
                        .draw(|f| {
                            f.render_widget(Block::new().style(theme::base()), f.area());
                            view.render(f, f.area());
                        })
                        .unwrap();
                    let buf = terminal.backend().buffer();
                    crate::ui::theme::testing::assert_buffer(buf);
                    let row = |y: u16| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>();
                    // The short card: names at the left, the time flush right
                    // in its timestamp colour on the header's surface.
                    let header = row(1);
                    assert!(header.starts_with("• Zed → Mateo"), "{header}");
                    assert!(header.ends_with(&sent), "{width}: {header}");
                    let surface =
                        theme::base().patch(theme::style(theme::Role::OtherConversationHeader));
                    let at = width - 1;
                    assert_eq!(buf[(at, 1)].bg, surface.bg.unwrap());
                    assert_eq!(
                        buf[(at, 1)].fg,
                        surface
                            .patch(theme::style(theme::Role::Timestamp))
                            .fg
                            .unwrap()
                    );
                    assert_eq!(row(2).trim(), "side body");
                    // The long card: the time on a row of its own at the
                    // right, the whole names below it, and the gutter
                    // marking exactly the header's rows.
                    let first = (3..20)
                        .find(|y| row(*y).trim_end().ends_with(&sent))
                        .unwrap();
                    // Only the bullet and the time on that row.
                    assert_eq!(row(first).replacen('•', " ", 1).trim(), sent, "{width}");
                    assert!(row(first + 1).contains("a-name-long"), "{width}");
                    let header_rows = (first..20)
                        .take_while(|y| !row(*y).contains("side body"))
                        .count();
                    assert!(header_rows >= 2);
                    let names: String = (first + 1..first + header_rows as u16).map(row).collect();
                    assert_eq!(
                        names
                            .matches("a-name-long-enough-to-crowd-the-time")
                            .count(),
                        2
                    );
                    let long_card = card(2, long, long);
                    // The gutter starts with the separator's blank row.
                    let gutter = long_card.gutter(false, None, width - 2);
                    assert_eq!(gutter.len() - 1, header_rows, "{width}");
                    assert_eq!(gutter[1].to_string(), "• ");
                }
            });
        }
    }

    #[test]
    fn reasoning_groups_get_one_blank_above_in_visible_time_order() {
        let mut stream = Stream::new("Mateo");
        let make = |stream: &mut Stream, id, event: &str, text: &str| {
            stream
                .entry(
                    &Kept::new(&proto::Activity {
                        id,
                        created_at: "2026-10-19T18:00:00Z".into(),
                        record: serde_json::json!({"event":event,"text":text}).to_string(),
                        ..Default::default()
                    }),
                    &Loaded::new(),
                )
                .unwrap()
        };
        let before = make(&mut stream, 10, "text", "Before.");
        let first = make(
            &mut stream,
            20,
            "thought",
            "**Checking output**\n\n**Checking bold**",
        );
        let second = make(&mut stream, 40, "thought", "Checking spacing");
        let after = make(&mut stream, 50, "text", "After.");
        let render = |view: &mut ThreadView<Entry>| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| {
                    view.render(frame, frame.area());
                })
                .unwrap();
            (0..view.frame.total as u16)
                .map(|y| {
                    (0..80)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let mut view = ThreadView::new(vec![before.clone(), first.clone()], 100);
        view.ingest(vec![second.clone(), after.clone()]);
        assert_eq!(
            render(&mut view),
            [
                "  Before.",
                "",
                "│ Checking output",
                "│ Checking bold",
                "│ Checking spacing",
                "  After."
            ]
        );
        // A late row splits the visible group, regardless of arrival order.
        let middle = make(&mut stream, 30, "text", "Interruption.");
        view.ingest(vec![middle.clone()]);
        let expected = [
            "  Before.",
            "",
            "│ Checking output",
            "│ Checking bold",
            "  Interruption.",
            "",
            "│ Checking spacing",
            "  After.",
        ];
        assert_eq!(render(&mut view), expected);
        let mut rebuilt = ThreadView::new(vec![before, first.clone(), middle, second, after], 100);
        assert_eq!(render(&mut rebuilt), expected);
        assert!(first.spaced_after(None), "a leading reasoning group");
        assert!(!first.spaced_last(), "no new trailing blank for reasoning");
        let padded = Entry::Stream {
            id: 1,
            created_at: String::new(),
            lines: vec![Line::raw(" \t")],
            markdown: None,
            call: None,
            thought: false,
            reply: false,
            echoes: Vec::new(),
        };
        assert!(
            !first.spaced_after(Some(&padded)),
            "reuse whitespace-only padding"
        );
        let tool = Entry::Stream {
            id: 2,
            created_at: String::new(),
            lines: vec![Line::raw("tool")],
            markdown: None,
            call: Some(ToolLine::running("read")),
            thought: false,
            reply: false,
            echoes: Vec::new(),
        };
        assert!(
            first.spaced_after(Some(&tool)),
            "tool closing and reasoning opening share one gap"
        );
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
                    other_participants: false,
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
                assert!((0..width).all(|x| buffer[(x, start)].symbol() == " "));
                // Unread, the bottom padding notes it at the right in the
                // quiet note color; with headers, the header says so instead.
                let bottom: String = (0..width).map(|x| buffer[(x, end - 1)].symbol()).collect();
                if compact {
                    assert!(bottom.ends_with("unread  "), "{bottom:?}");
                    assert_eq!(bottom.trim(), "unread");
                    let note = theme::style(theme::Role::HumanNote);
                    for x in width - 8..width - 2 {
                        let cell = &buffer[(x, end - 1)];
                        assert_eq!(cell.fg, note.fg.or(human.fg).unwrap(), "{scheme:?}");
                        assert_eq!(cell.bg, human.bg.unwrap());
                        if scheme != crate::ui::theme::Scheme::NoColor {
                            assert_ne!(cell.fg, human.fg.unwrap(), "a quieter color");
                            assert_ne!(cell.fg, cell.bg);
                        } else {
                            assert!(cell.modifier.contains(Modifier::DIM));
                        }
                    }
                } else {
                    assert!(bottom.trim().is_empty(), "{bottom:?}");
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

    #[test]
    fn tables_reflow_in_mail_replies_and_reasoning_without_replaying_state() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        let source = format!(
            "| Name | Detail |\n| :--- | ---: |\n| **Choice** | {} tailtoken |\n",
            "Unicode 界 café e\u{301} ".repeat(12)
        );
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut rows = Vec::new();
                for (headerless, outgoing, side) in [
                    (true, false, false),
                    (true, true, false),
                    (false, false, false),
                    (false, false, true),
                ] {
                    rows.push(Entry::Message {
                        message: Message {
                            id: rows.len() as i64 + 1,
                            body: source.clone(),
                            from_name: "A".into(),
                            to_name: Some("B".into()),
                            ..Default::default()
                        },
                        headerless,
                        outgoing,
                        recipient: Some(proto::Kind::Agent),
                        other_participants: side,
                        gap: false,
                    });
                }
                let mut stream = Stream::new("Mateo");
                let running = Kept::new(&proto::Activity {
                    id: 20,
                    record: r#"{"event":"tool","id":"one","title":"read","status":"pending"}"#
                        .into(),
                    ..Default::default()
                });
                stream.entry(&running, &Loaded::new());
                let before = stream.running(80);
                assert!(!before.is_empty());
                for (id, kind) in [(21, "text"), (22, "thought")] {
                    let raw = serde_json::json!({"event":kind,"text":source}).to_string();
                    let kept = Kept::new(&proto::Activity {
                        id,
                        record: raw.clone(),
                        ..Default::default()
                    });
                    rows.push(stream.entry(&kept, &Loaded::new()).unwrap());
                    assert_eq!(kept.activity.record, raw);
                }
                for row in rows {
                    let mut heights = Vec::new();
                    let mut view = ThreadView::new(vec![row.clone()], 100);
                    view.set_query("choice");
                    assert_eq!(view.search_status(), Some(("choice", 1)));
                    for width in [60, 100, 72, 60] {
                        view.follow();
                        let inner = width - row.inset();
                        let lines = row.lines_at(false, Some("choice"), inner);
                        assert!(
                            lines.iter().all(|l| l.width() <= usize::from(inner)),
                            "{width}: {lines:?}"
                        );
                        assert!(lines.iter().any(|l| l.to_string().contains("tailtoken")));
                        let height = Paragraph::new(lines.clone())
                            .wrap(Wrap { trim: false })
                            .line_count(inner);
                        heights.push(height);
                        if row.is_thought() {
                            assert_eq!(row.gutter(false, None, inner).len(), height);
                            assert!(
                                lines
                                    .iter()
                                    .flat_map(|l| &l.spans)
                                    .all(|s| s.style.add_modifier.contains(Modifier::ITALIC))
                            );
                        }
                        let mut terminal = Terminal::new(TestBackend::new(width, 200)).unwrap();
                        terminal
                            .draw(|f| {
                                f.render_widget(Block::new().style(theme::base()), f.area());
                                view.render(f, f.area());
                            })
                            .unwrap();
                        assert!(view.frame.total >= height);
                        crate::ui::theme::testing::assert_buffer(terminal.backend().buffer());
                        view.layout(width, 5);
                        view.scroll_to(2);
                        assert!(!view.following());
                        view.layout(100, 5);
                        assert_eq!(view.paused.unwrap().id, row.key());
                    }
                    assert!(heights[0] > heights[1], "table cells reflow: {heights:?}");
                    assert_eq!(heights[0], heights[3], "resize back restores geometry");
                }
                assert_eq!(
                    stream.running(80),
                    before,
                    "content reflow cannot touch tool lifecycle"
                );
            });
        }
    }
}
