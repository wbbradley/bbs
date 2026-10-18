//! Agents: yours by default, everyone's on request, each with its owner.
//! Those with mail to the viewer unread come first, then those with activity
//! unseen, each oldest first.

use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};

use super::thread::Thread;
use super::{fit, fit_styled_list, navigate};
use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::event::{Data, Request};
use crate::connect::keys::Action;
use crate::connect::theme;
use crate::proto;
use crate::ui::message::age;

pub struct Agents {
    /// None until the first reply.
    agents: Option<Result<Vec<proto::Agent>, String>>,
    /// The selected agent, kept selected as the order changes.
    selected: Option<String>,
    /// Everyone's agents rather than only the viewer's.
    all: bool,
    /// A fixed render time for tests; live renders use the current time.
    now: Option<DateTime<Local>>,
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Default for Agents {
    fn default() -> Agents {
        Agents {
            agents: None,
            selected: None,
            all: false,
            now: None,
            state: TableState::default(),
            page: 0,
        }
    }
}

impl Agents {
    /// Fix the render time for tests.
    #[cfg(test)]
    pub fn set_now(&mut self, now: DateTime<Local>) {
        self.now = Some(now);
    }

    /// Unread mail first, then unseen activity, each oldest first; the rest
    /// keep the server's order.
    fn shown<'a>(&'a self, ctx: &Ctx) -> Vec<&'a proto::Agent> {
        let Some(Ok(agents)) = &self.agents else {
            return Vec::new();
        };
        let mut shown: Vec<_> = agents
            .iter()
            .filter(|a| self.all || a.owner.as_deref() == Some(ctx.me.as_str()))
            .collect();
        shown.sort_by_key(
            |a| match ctx.inbox.summaries.get(&a.name.to_ascii_lowercase()) {
                Some(s) if s.unread_messages > 0 => (0, s.oldest_unread_message.clone()),
                Some(s) if s.unseen_stream > 0 => (1, s.oldest_unseen_stream.clone()),
                _ => (2, None),
            },
        );
        shown
    }

    /// Remember the selected agent's name.
    fn remember(&mut self, ctx: &Ctx) {
        let name = self
            .state
            .selected()
            .and_then(|i| self.shown(ctx).get(i).map(|a| a.name.clone()));
        self.selected = name;
    }

    /// Select the remembered agent wherever it now is.
    fn reselect(&mut self, ctx: &Ctx) {
        let at = self
            .selected
            .as_ref()
            .and_then(|name| self.shown(ctx).iter().position(|a| &a.name == name));
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
        format!(
            "{marker}{}{}",
            if marker == '✉' { " " } else { "" },
            count.min(99)
        )
    }
}

impl Screen for Agents {
    #[cfg(test)]
    fn as_agents_mut(&mut self) -> Option<&mut Agents> {
        Some(self)
    }

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
            if self.state.selected().is_none() {
                self.state.select(Some(0));
            }
        }
    }

    fn handle(&mut self, action: Action, ctx: &Ctx) -> Transition {
        let len = self.shown(ctx).len();
        if navigate(&mut self.state, len, self.page, action) {
            self.remember(ctx);
            return Transition::Stay;
        }
        match action {
            Action::ToggleAll => {
                self.all = !self.all;
                self.state.select(Some(0));
                self.remember(ctx);
                Transition::Stay
            }
            Action::Open => {
                let shown = self.shown(ctx);
                match self.state.selected().and_then(|i| shown.get(i)) {
                    Some(a) => Transition::Push(Box::new(Thread::agent(
                        &a.name,
                        &ctx.me,
                        ctx.preferences.compact(),
                        ctx.now.clone(),
                    ))),
                    None => Transition::Stay,
                }
            }
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.reselect(ctx);
        let shown = self.shown(ctx);
        let title = match (self.all, &self.agents) {
            (false, Some(Ok(_))) => format!("Your agents ({}) · a: everyone's", shown.len()),
            (true, Some(Ok(_))) => format!("All agents ({}) · a: yours", shown.len()),
            _ => "Agents".to_string(),
        };
        let inner = theme::screen(frame, area, &title);
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
        // One render time for every row in the frame. ACTIVE is the age of the
        // agent's last sent message or activity event, live from the summary
        // stream when it has one and otherwise from the list; an agent without
        // any (or a server too old to say) shows a dash rather than a false age.
        let now = self.now.unwrap_or_else(Local::now);
        let ages: Vec<String> = shown
            .iter()
            .map(|a| {
                let sent = ctx
                    .inbox
                    .summaries
                    .get(&a.name.to_ascii_lowercase())
                    .and_then(|s| s.last_sent_at.as_deref())
                    .or(a.last_sent_at.as_deref());
                match sent.map(|s| age(s, now)) {
                    Some(s) if !s.is_empty() => s,
                    _ => "-".to_string(),
                }
            })
            .collect();
        // Keep both counts and a readable status at the minimum terminal width.
        // AGENT fits the whole random name pool; OWNER fits most human names.
        // ACTIVE fits the widest rendered age ("103d23h ago" is 11), at the
        // header's 6 minimum and at most 14 so STATUS keeps 10 columns.
        let active_width = ages.iter().map(|a| a.len()).max().unwrap_or(6).clamp(6, 14) as u16;
        let groups_width = inner
            .width
            .saturating_sub(10 + 9 + 5 + 8 + active_width + 10 + 6)
            .clamp(11, 16);
        let status_width = inner
            .width
            .saturating_sub(10 + 9 + 5 + 8 + groups_width + active_width + 6)
            as usize;
        let rows = shown.iter().zip(&ages).map(|(a, age)| {
            let owner = a.owner.as_deref().unwrap_or("?");
            let author_style = theme::style(theme::Role::Agent);
            let name = Line::from(vec![Span::styled(
                a.name.clone(),
                author_style.add_modifier(Modifier::BOLD),
            )]);
            let owner = Line::from(vec![Span::styled(
                owner.to_string(),
                theme::style(theme::Role::Human),
            )]);
            let summary = ctx.inbox.summaries.get(&a.name.to_ascii_lowercase());
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
                fit_styled_list(
                    a.groups
                        .iter()
                        .map(|g| Line::styled(g.clone(), theme::style(theme::Role::Group)))
                        .collect(),
                    groups_width as usize,
                ),
                Line::styled(age.clone(), theme::style(theme::Role::Timestamp)),
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
                Constraint::Length(active_width),
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
        .row_highlight_style(ratatui::style::Style {
            fg: None,
            ..theme::style(theme::Role::Selection)
        });
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height.saturating_sub(1) as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
        self.remember(ctx);
    }
}
