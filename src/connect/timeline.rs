//! A thread's timeline: messages and, in an agent's thread, the activity it
//! shares, drawn as `bbs headless logs --pretty` draws it, in time order.

use std::collections::VecDeque;

use chrono::DateTime;
use ratatui::style::Style;
use ratatui::text::Line;

use super::app::next_id;
use super::event::{Data, Request};
use super::theme;
use crate::backend::AGENT_ACTIVITY;
use crate::proto::{self, Message};
use crate::ui::activity::Activity;
use crate::ui::message::{compact_message_lines, find_matches, message_lines};
use crate::ui::thread::Row;

/// Events each batch holds; the stream catches up from the start in batches.
const BACKLOG: u32 = 200;
/// Activity records kept; the oldest go first.
const MAX_RECORDS: usize = 5000;

/// One row of a timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Message {
        message: Message,
        /// Omit message headers in compact conversations.
        compact: bool,
        outgoing: bool,
        /// A possible gap in the raw feed before this visible message.
        gap: bool,
    },
    /// One activity record, drawn when it arrived.
    Stream {
        id: i64,
        created_at: String,
        lines: Vec<Line<'static>>,
    },
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
                compact,
                outgoing,
                gap: raw_gap,
            } => {
                let mut lines = if *compact {
                    compact_message_lines(message, gap || *raw_gap, query)
                } else {
                    message_lines(message, gap, query)
                };
                if *outgoing {
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
                compact: true,
                outgoing: true,
                ..
            }
        )
        .then(|| theme::style(theme::Role::Screen))
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

    fn shows(self, record: &str) -> bool {
        let event = serde_json::from_str::<serde_json::Value>(record)
            .ok()
            .and_then(|r| r["event"].as_str().map(str::to_string))
            .unwrap_or_default();
        match self {
            Kinds::All => true,
            Kinds::NoText => !matches!(event.as_str(), "text" | "thought"),
            Kinds::Problems => {
                matches!(event.as_str(), "warning" | "error" | "turn_error" | "stop")
            }
        }
    }
}

/// An agent's shared activity, as it streams in.
pub struct Stream {
    id: u64,
    agent: String,
    pub kinds: Kinds,
    /// Every record received, so another filter can redraw them all.
    records: VecDeque<proto::Activity>,
    renderer: Activity,
    /// A batch has arrived, so no records means nothing is shared.
    pub loaded: bool,
    /// The stream's last error; it has ended until a reload.
    pub error: Option<String>,
}

impl Stream {
    pub fn new(agent: &str) -> Stream {
        Stream {
            id: next_id(),
            agent: agent.to_string(),
            kinds: Kinds::All,
            records: VecDeque::new(),
            renderer: Activity::new(true).local_time(),
            loaded: false,
            error: None,
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
            id: self.id,
            kinds: self.kinds,
            ..Stream::new(&self.agent)
        };
    }

    /// The newest record received.
    pub fn newest(&self) -> Option<i64> {
        self.records.back().map(|r| r.id)
    }

    /// Tool calls still running, to show below the newest activity.
    pub fn running(&self) -> Vec<Line<'static>> {
        self.renderer.in_progress()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Draw `record` as an entry, if the kinds shown include it.
    fn entry(&mut self, record: &proto::Activity) -> Option<Entry> {
        if !self.kinds.shows(&record.record) {
            return None;
        }
        let lines = self.renderer.render(&record.record);
        (!lines.is_empty()).then(|| Entry::Stream {
            id: record.id,
            created_at: record.created_at.clone(),
            lines,
        })
    }

    /// Every record kept, drawn afresh, as after changing the kinds.
    pub fn entries(&mut self) -> Vec<Entry> {
        self.renderer = Activity::new(true).local_time();
        let records = std::mem::take(&mut self.records);
        let entries = records.iter().filter_map(|r| self.entry(r)).collect();
        self.records = records;
        entries
    }

    /// Take a batch of this stream's data; returns the new entries.
    pub fn on_data(&mut self, data: &Data) -> Vec<Entry> {
        let Data::Activity { id, result } = data else {
            return Vec::new();
        };
        if *id != self.id {
            return Vec::new();
        }
        match result {
            Ok(events) => {
                self.loaded = true;
                self.error = None;
                let entries = events.iter().filter_map(|e| self.entry(e)).collect();
                self.records.extend(events.iter().cloned());
                let over = self.records.len().saturating_sub(MAX_RECORDS);
                self.records.drain(..over);
                entries
            }
            Err(e) if e.contains(AGENT_ACTIVITY) => {
                self.error = Some(format!(
                    "Activity needs a newer server ({AGENT_ACTIVITY}). Ask its operator to upgrade."
                ));
                Vec::new()
            }
            Err(e) => {
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
    use ratatui::{Terminal, backend::TestBackend, style::Modifier, widgets::Block};

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
                    compact,
                    gap: false,
                };
                let outgoing = make(
                    2,
                    true,
                    "**needle** followed by enough words to wrap across several full rendered rows.\n\n`code` and [link](https://example.com)",
                );
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
