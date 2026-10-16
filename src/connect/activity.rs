//! The activity pane: an agent's shared activity, live, drawn as `bbs
//! headless logs --pretty` draws it.

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
/// Lines kept; the oldest go first.
const MAX_LINES: usize = 5000;

pub struct ActivityPane {
    id: u64,
    agent: String,
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
            ..ActivityPane::new(&self.agent)
        };
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
                for event in events {
                    self.lines.extend(self.renderer.render(&event.record));
                }
                self.lines.extend(self.renderer.running());
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
            .title(Span::styled(" Activity · A hides ", theme::heading()))
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
        if self.lines.is_empty() {
            let text = format!(
                "{} shares no activity. A headless agent shares it only when its harness sets share_activity = true; see HEADLESS.md.",
                self.agent
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
