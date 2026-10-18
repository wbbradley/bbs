//! Agents: yours by default, everyone's on request, each with its owner.
//! Those with mail to the viewer unread come first, then those with activity
//! unseen, each oldest first.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use std::collections::HashMap;

use super::thread::Thread;
use super::{fit, fit_list, navigate};
use crate::connect::app::{Ctx, Screen, Transition, next_id};
use crate::connect::event::{Data, Request};
use crate::connect::keys::Action;
use crate::connect::theme;
use crate::proto;
use crate::ui::message::local_time;

pub struct Agents {
    /// The summary stream.
    id: u64,
    /// None until the first reply.
    agents: Option<Result<Vec<proto::Agent>, String>>,
    /// What the viewer has yet to read or see, by lower-cased agent name. An
    /// older server has no summaries; the list keeps the server's order.
    summaries: HashMap<String, proto::AgentSummary>,
    /// The selected agent, kept selected as the order changes.
    selected: Option<String>,
    /// Everyone's agents rather than only the viewer's.
    all: bool,
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Default for Agents {
    fn default() -> Agents {
        Agents {
            id: next_id(),
            agents: None,
            summaries: HashMap::new(),
            selected: None,
            all: false,
            state: TableState::default(),
            page: 0,
        }
    }
}

impl Agents {
    fn summary(&self, agent: &str) -> Option<&proto::AgentSummary> {
        self.summaries.get(&agent.to_lowercase())
    }

    /// Unread mail first, then unseen activity, each oldest first; the rest
    /// keep the server's order.
    fn shown<'a>(&'a self, me: &str) -> Vec<&'a proto::Agent> {
        let Some(Ok(agents)) = &self.agents else {
            return Vec::new();
        };
        let mut shown: Vec<_> = agents
            .iter()
            .filter(|a| self.all || a.owner.as_deref() == Some(me))
            .collect();
        shown.sort_by_key(|a| match self.summary(&a.name) {
            Some(s) if s.unread_messages > 0 => (0, s.oldest_unread_message.clone()),
            Some(s) if s.unseen_stream > 0 => (1, s.oldest_unseen_stream.clone()),
            _ => (2, None),
        });
        shown
    }

    /// Remember the selected agent's name.
    fn remember(&mut self, me: &str) {
        let name = self
            .state
            .selected()
            .and_then(|i| self.shown(me).get(i).map(|a| a.name.clone()));
        self.selected = name;
    }

    /// Select the remembered agent wherever it now is.
    fn reselect(&mut self, me: &str) {
        let at = self
            .selected
            .as_ref()
            .and_then(|name| self.shown(me).iter().position(|a| &a.name == name));
        if let Some(i) = at {
            self.state.select(Some(i));
        }
    }
}

/// Count conventions shared by the inbox and activity columns: blank at zero,
/// otherwise the source marker and a count capped at 99.
fn pending(count: u32, marker: char) -> String {
    if count == 0 {
        String::new()
    } else {
        format!("{marker}{}", count.min(99))
    }
}

impl Screen for Agents {
    fn title(&self) -> String {
        "Agents".into()
    }

    fn requests(&self) -> Vec<Request> {
        vec![Request::Agents, Request::WatchSummary(self.id)]
    }

    fn on_close(&self) -> Vec<Request> {
        vec![Request::Unwatch(self.id)]
    }

    fn on_data(&mut self, data: &Data) {
        if let Data::Summary { id, result } = data
            && *id == self.id
        {
            // Without summaries (an older server, say) the order is the server's.
            self.summaries = match result {
                Ok(summaries) => summaries
                    .iter()
                    .map(|s| (s.agent.to_lowercase(), s.clone()))
                    .collect(),
                Err(_) => HashMap::new(),
            };
        }
        if let Data::Agents(result) = data {
            self.agents = Some(
                result
                    .as_ref()
                    .map(|r| r.agents.clone())
                    .map_err(Clone::clone),
            );
            if self.state.selected().is_none() {
                self.state.select(Some(0));
            }
        }
    }

    fn handle(&mut self, action: Action, ctx: &Ctx) -> Transition {
        let len = self.shown(&ctx.me).len();
        if navigate(&mut self.state, len, self.page, action) {
            self.remember(&ctx.me);
            return Transition::Stay;
        }
        match action {
            Action::ToggleAll => {
                self.all = !self.all;
                self.state.select(Some(0));
                self.remember(&ctx.me);
                Transition::Stay
            }
            Action::Open => {
                let shown = self.shown(&ctx.me);
                match self.state.selected().and_then(|i| shown.get(i)) {
                    Some(a) => Transition::Push(Box::new(Thread::agent(
                        &a.name,
                        a.owner.as_deref(),
                        &ctx.me,
                        ctx.preferences.compact(),
                    ))),
                    None => Transition::Stay,
                }
            }
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.reselect(&ctx.me);
        let shown = self.shown(&ctx.me);
        let title = match (self.all, &self.agents) {
            (false, Some(Ok(_))) => format!("Your agents ({}) · a: everyone's", shown.len()),
            (true, Some(Ok(_))) => format!("All agents ({}) · a: yours", shown.len()),
            _ => "Agents".to_string(),
        };
        let window = theme::window(&title);
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let empty = match &self.agents {
            None => Some(Line::styled("Loading…", theme::dim())),
            Some(Err(e)) => Some(Line::styled(e.clone(), theme::error())),
            Some(Ok(_)) if shown.is_empty() => Some(Line::styled(
                "No agents. Press a for everyone's.",
                theme::dim(),
            )),
            Some(Ok(_)) => None,
        };
        if let Some(line) = empty {
            frame.render_widget(Paragraph::new(line), inner);
            return;
        }
        // Keep both counts and a readable status at the minimum terminal width.
        // AGENT fits the whole random name pool; OWNER fits most human names.
        let groups_width = inner
            .width
            .saturating_sub(10 + 9 + 5 + 8 + 19 + 10 + 6)
            .clamp(11, 16);
        let status_width = inner
            .width
            .saturating_sub(10 + 9 + 5 + 8 + groups_width + 19 + 6) as usize;
        let rows = shown.iter().map(|a| {
            let owner = a.owner.as_deref().unwrap_or("?");
            let color = if owner == ctx.me {
                theme::OWN_AGENT
            } else {
                theme::OTHER_AGENT
            };
            let name = Line::from(vec![Span::styled(
                a.name.clone(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )]);
            let owner = Line::from(vec![Span::styled(owner.to_string(), Style::new().fg(color))]);
            let summary = self.summary(&a.name);
            Row::new(vec![
                name,
                owner,
                Line::styled(
                    pending(summary.map_or(0, |s| s.unread_messages), '✉'),
                    theme::heading(),
                ),
                Line::styled(
                    pending(summary.map_or(0, |s| s.unseen_stream), '•'),
                    theme::heading(),
                ),
                Line::raw(fit_list(&a.groups, groups_width as usize)),
                Line::raw(local_time(&a.active_at)),
                Line::raw(fit(a.status.as_deref().unwrap_or_default(), status_width)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Length(8),
                Constraint::Length(groups_width),
                Constraint::Length(19),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new([
                "AGENT", "OWNER", "INBOX", "ACTIVITY", "GROUPS", "ACTIVE", "STATUS",
            ])
            .style(theme::heading()),
        )
        .style(theme::base())
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height.saturating_sub(1) as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
        self.remember(&ctx.me);
    }
}
