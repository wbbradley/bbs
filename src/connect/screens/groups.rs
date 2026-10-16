//! Groups, with their descriptions, members and writers.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use super::thread::Thread;
use super::{fit, fit_list, navigate};
use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::event::{Data, Request};
use crate::connect::keys::Action;
use crate::connect::theme;
use crate::proto;

#[derive(Default)]
pub struct Groups {
    /// None until the first reply.
    groups: Option<Result<Vec<proto::Group>, String>>,
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

/// Members in order, writers marked `[w]` as in `bbs group list`.
fn members(group: &proto::Group) -> Vec<String> {
    let writers = group.writers.as_ref().map(|w| &w.names);
    group
        .members
        .iter()
        .map(|m| match writers.is_some_and(|w| w.contains(m)) {
            true => format!("{m} [w]"),
            false => m.clone(),
        })
        .collect()
}

impl Screen for Groups {
    fn title(&self) -> String {
        "Groups".into()
    }

    fn requests(&self) -> Vec<Request> {
        vec![Request::Groups]
    }

    fn on_data(&mut self, data: &Data) {
        if let Data::Groups(result) = data {
            self.groups = Some(
                result
                    .as_ref()
                    .map(|r| r.groups.clone())
                    .map_err(Clone::clone),
            );
            if self.state.selected().is_none() {
                self.state.select(Some(0));
            }
        }
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        let len = match &self.groups {
            Some(Ok(groups)) => groups.len(),
            _ => 0,
        };
        if navigate(&mut self.state, len, self.page, action) {
            return Transition::Stay;
        }
        match action {
            Action::Open => match (&self.groups, self.state.selected()) {
                (Some(Ok(groups)), Some(i)) if i < groups.len() => {
                    Transition::Push(Box::new(Thread::group(&groups[i].name)))
                }
                _ => Transition::Stay,
            },
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        let title = match &self.groups {
            Some(Ok(groups)) => format!("Groups ({})", groups.len()),
            _ => "Groups".to_string(),
        };
        let window = theme::window(&title);
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let groups = match &self.groups {
            None => {
                let line = Line::styled("Loading…", theme::dim());
                return frame.render_widget(Paragraph::new(line), inner);
            }
            Some(Err(e)) => {
                let line = Line::styled(e.clone(), theme::error());
                return frame.render_widget(Paragraph::new(line), inner);
            }
            Some(Ok(groups)) if groups.is_empty() => {
                let line = Line::styled("No groups yet.", theme::dim());
                return frame.render_widget(Paragraph::new(line), inner);
            }
            Some(Ok(groups)) => groups,
        };
        let members_width = inner.width.saturating_sub(16 + 30 + 2) as usize;
        let rows = groups.iter().map(|g| {
            Row::new(vec![
                Line::styled(
                    format!("@{}", g.name),
                    Style::new()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::raw(fit(g.description.as_deref().unwrap_or_default(), 30)),
                Line::raw(fit_list(&members(g), members_width)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(30),
                Constraint::Min(10),
            ],
        )
        .header(Row::new(["GROUP", "DESCRIPTION", "MEMBERS ([w] writes)"]).style(theme::heading()))
        .style(theme::base())
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height.saturating_sub(1) as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
    }
}
