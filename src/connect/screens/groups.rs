//! Groups, with their descriptions, members and writers, and the dialogs that
//! change them.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use super::thread::Thread;
use super::{fit, fit_styled_list, navigate};
use crate::connect::admin::GroupAdmin;
use crate::connect::app::{Ctx, Screen, Transition, next_id};
use crate::connect::event::{Data, Request};
use crate::connect::keys::Action;
use crate::connect::theme;
use crate::proto;

pub struct Groups {
    id: u64,
    admin: GroupAdmin,
    /// None until the first reply.
    groups: Option<Result<Vec<proto::Group>, String>>,
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Groups {
    pub fn new() -> Groups {
        Groups {
            id: next_id(),
            admin: GroupAdmin::default(),
            groups: None,
            state: TableState::default(),
            page: 0,
        }
    }

    fn selected(&self) -> Option<&proto::Group> {
        match &self.groups {
            Some(Ok(groups)) => groups.get(self.state.selected()?),
            _ => None,
        }
    }
}

/// Group members are agent names; the writer flag is a separate status.
fn members(group: &proto::Group, width: usize) -> Line<'static> {
    let writers = group.writers.as_ref().map(|w| &w.names);
    fit_styled_list(
        group
            .members
            .iter()
            .map(|name| {
                let mut spans = vec![Span::styled(name.clone(), theme::style(theme::Role::Agent))];
                if writers.is_some_and(|w| w.contains(name)) {
                    spans.push(Span::styled(" [w]", theme::style(theme::Role::Success)));
                }
                Line::from(spans)
            })
            .collect(),
        width,
    )
}

impl Screen for Groups {
    fn navigation_blocked(&self) -> bool {
        self.admin.is_open()
    }

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
            // A deleted group leaves the list; keep the selection in it.
            let len = match &self.groups {
                Some(Ok(groups)) => groups.len(),
                _ => 0,
            };
            let selected = self.state.selected().unwrap_or(0);
            self.state.select(Some(selected.min(len.saturating_sub(1))));
        }
        self.admin.on_data(self.id, data);
    }

    fn raw_key(&mut self, key: KeyEvent) -> Option<Transition> {
        self.admin.key(self.id, key)
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
            Action::Group(op) => {
                let group = self.selected().cloned();
                self.admin.open(op, group.as_ref());
                Transition::Stay
            }
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
        self.draw(frame, area);
        self.admin.render(frame, area);
    }
}

impl Groups {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let title = match &self.groups {
            Some(Ok(groups)) => format!("Groups ({})", groups.len()),
            _ => "Groups".to_string(),
        };
        let inner = theme::screen(frame, area, &title);
        let [inner, notice] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let hint = Line::styled(
            "c create · e describe · D delete · +/- add/remove · w/W grant/revoke write",
            theme::dim(),
        );
        frame.render_widget(Paragraph::new(self.admin.notice().unwrap_or(hint)), notice);
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
                    theme::style(theme::Role::Group).add_modifier(Modifier::BOLD),
                ),
                Line::raw(fit(g.description.as_deref().unwrap_or_default(), 30)),
                members(g, members_width),
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
        .row_highlight_style(ratatui::style::Style {
            fg: None,
            ..theme::style(theme::Role::Selection)
        });
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height.saturating_sub(1) as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
    }
}
