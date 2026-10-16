//! The activity pane: an agent's shared activity, live, drawn as `bbs
//! headless logs --pretty` draws it, optionally only some kinds of event.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use super::app::next_id;
use super::event::{Data, Request};
use super::theme;
use crate::backend::AGENT_ACTIVITY;
use crate::proto;
use crate::ui::activity::Activity;

/// Events each batch holds; the stream catches up from the start in batches.
const BACKLOG: u32 = 200;
/// Records and drawn lines kept; the oldest go first.
const MAX_RECORDS: usize = 5000;
const MAX_LINES: usize = 10_000;

/// Which events the pane shows; `E` cycles through them.
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
    fn next(self) -> Kinds {
        match self {
            Kinds::All => Kinds::NoText,
            Kinds::NoText => Kinds::Problems,
            Kinds::Problems => Kinds::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kinds::All => "all",
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

pub struct ActivityPane {
    id: u64,
    agent: String,
    kinds: Kinds,
    /// Every record received, so another filter can redraw them all.
    records: std::collections::VecDeque<String>,
    renderer: Activity,
    lines: Vec<Line<'static>>,
    /// A batch has arrived, so an empty pane means nothing is shared.
    loaded: bool,
    /// The stream's error; it has ended until a reload.
    error: Option<String>,
}

impl ActivityPane {
    pub fn new(agent: &str) -> ActivityPane {
        ActivityPane {
            id: next_id(),
            agent: agent.to_string(),
            kinds: Kinds::All,
            records: std::collections::VecDeque::new(),
            renderer: Activity::new(true),
            lines: Vec::new(),
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
        *self = ActivityPane {
            id: self.id,
            kinds: self.kinds,
            ..ActivityPane::new(&self.agent)
        };
    }

    /// Show the next set of kinds, redrawing every record kept.
    pub fn cycle(&mut self) {
        self.kinds = self.kinds.next();
        self.renderer = Activity::new(true);
        self.lines.clear();
        let records = std::mem::take(&mut self.records);
        self.show(records.iter());
        self.records = records;
    }

    /// Render `records` the filter shows, then any tool calls still running.
    fn show<'r>(&mut self, records: impl Iterator<Item = &'r String>) {
        for record in records.filter(|r| self.kinds.shows(r)) {
            self.lines.extend(self.renderer.render(record));
        }
        self.lines.extend(self.renderer.running());
    }

    pub fn on_data(&mut self, data: &Data) {
        let Data::Activity { id, result } = data else {
            return;
        };
        if *id != self.id {
            return;
        }
        match result {
            Ok(events) => {
                self.loaded = true;
                self.error = None;
                let records: Vec<String> = events.iter().map(|e| e.record.clone()).collect();
                self.show(records.iter());
                self.records.extend(records);
                let over = self.records.len().saturating_sub(MAX_RECORDS);
                self.records.drain(..over);
                let over = self.lines.len().saturating_sub(MAX_LINES);
                self.lines.drain(..over);
            }
            Err(e) if e.contains(AGENT_ACTIVITY) => {
                self.error = Some(format!(
                    "Activity needs a newer server ({AGENT_ACTIVITY}). Ask its operator to upgrade."
                ));
            }
            Err(e) => self.error = Some(e.clone()),
        }
    }

    /// Draw the newest lines that fit in `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::dim())
            .title(Span::styled(
                format!(" Activity · {} · E kinds · A hides ", self.kinds.label()),
                theme::heading(),
            ))
            .style(theme::base());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let notice = |text: String, style| {
            Paragraph::new(Line::styled(text, style)).wrap(Wrap { trim: false })
        };
        if let Some(error) = &self.error {
            frame.render_widget(notice(format!("{error} r retries."), theme::error()), inner);
            return;
        }
        if !self.loaded {
            frame.render_widget(notice("Loading…".into(), theme::dim()), inner);
            return;
        }
        if self.records.is_empty() {
            let text = format!(
                "{} shares no activity. A headless agent shares it only when its harness sets share_activity = true; see HEADLESS.md.",
                self.agent
            );
            frame.render_widget(notice(text, theme::dim()), inner);
            return;
        }
        if self.lines.is_empty() {
            let text = format!(
                "Nothing of these kinds. E shows {}.",
                self.kinds.next().label()
            );
            frame.render_widget(notice(text, theme::dim()), inner);
            return;
        }
        // Each line takes at least one row, so the newest `height` fill it.
        let from = self.lines.len().saturating_sub(inner.height as usize);
        let paragraph = Paragraph::new(self.lines[from..].to_vec()).wrap(Wrap { trim: false });
        let total = paragraph.line_count(inner.width);
        let top = total.saturating_sub(inner.height as usize);
        frame.render_widget(
            paragraph.scroll((top.min(u16::MAX as usize) as u16, 0)),
            inner,
        );
    }
}
