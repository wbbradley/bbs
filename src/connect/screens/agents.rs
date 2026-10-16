//! Agents: yours by default, everyone's on request, each with its owner.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use super::navigate;
use crate::app::display_time;
use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::event::{Data, Request};
use crate::connect::keys::Action;
use crate::connect::theme;
use crate::proto;

#[derive(Default)]
pub struct Agents {
    /// None until the first reply.
    agents: Option<Result<Vec<proto::Agent>, String>>,
    /// Everyone's agents rather than only the viewer's.
    all: bool,
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Agents {
    fn shown<'a>(&'a self, me: &str) -> Vec<&'a proto::Agent> {
        let Some(Ok(agents)) = &self.agents else {
            return Vec::new();
        };
        agents
            .iter()
            .filter(|a| self.all || a.owner.as_deref() == Some(me))
            .collect()
    }
}

impl Screen for Agents {
    fn title(&self) -> String {
        "Agents".into()
    }

    fn requests(&self) -> Vec<Request> {
        vec![Request::Agents]
    }

    fn on_data(&mut self, data: &Data) {
        if let Data::Agents(result) = data {
            self.agents = Some(
                result
                    .as_ref()
                    .map(|r| r.agents.clone())
                    .map_err(Clone::clone),
            );
        }
    }

    fn handle(&mut self, action: Action, ctx: &Ctx) -> Transition {
        let len = self.shown(&ctx.me).len();
        if navigate(&mut self.state, len, self.page, action) {
            return Transition::Stay;
        }
        match action {
            Action::ToggleAll => {
                self.all = !self.all;
                self.state.select(Some(0));
                Transition::Stay
            }
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
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
        let rows = shown.iter().map(|a| {
            let owner = a.owner.as_deref().unwrap_or("?");
            let color = if owner == ctx.me {
                theme::OWN_AGENT
            } else {
                theme::OTHER_AGENT
            };
            let name = Line::from(vec![
                Span::styled(
                    a.name.clone(),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("@{owner}"), Style::new().fg(color)),
            ]);
            Row::new(vec![
                name,
                Line::raw(a.groups.join(", ")),
                Line::raw(display_time(&a.active_at)),
                Line::raw(a.status.clone().unwrap_or_default()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(22),
                Constraint::Length(16),
                Constraint::Length(20),
                Constraint::Min(10),
            ],
        )
        .header(Row::new(["AGENT", "GROUPS", "ACTIVE", "STATUS"]).style(theme::heading()))
        .style(theme::base())
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height.saturating_sub(1) as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
    }
}
