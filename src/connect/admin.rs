//! Group admin: the dialogs that change a group, shared by the Groups list and
//! a group's thread. Each maps onto one group RPC, under the CLI's rules.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::app::Transition;
use super::dialog::{Dialog, Outcome};
use super::event::{Admin, Data, Notice, Request};
use super::keys::GroupOp;
use super::theme;
use crate::proto;

#[derive(Default)]
pub struct GroupAdmin {
    /// The open dialog, the change it makes, and its group (empty to create).
    dialog: Option<(GroupOp, String, Dialog)>,
    /// What the last change did, or why it failed.
    notice: Option<Result<Notice, String>>,
}

impl GroupAdmin {
    /// Open the dialog for `op` on `group`; creating needs none.
    pub fn open(&mut self, op: GroupOp, group: Option<&proto::Group>) {
        let name = group.map(|g| g.name.clone()).unwrap_or_default();
        if op != GroupOp::Create && name.is_empty() {
            return;
        }
        let at = format!("@{name}");
        let title = |prefix: &'static str| {
            Line::from(vec![
                Span::raw(prefix),
                Span::styled(at.clone(), theme::style(theme::Role::Group)),
            ])
        };
        let dialog = match op {
            GroupOp::Create => Dialog::form(
                "Create a group",
                "A new group. Agents join it, or you add them.",
                vec![("name", String::new()), ("description", String::new())],
            ),
            GroupOp::Describe => Dialog::form(
                title("Describe "),
                "One line, at most 200 characters. Empty clears it.",
                vec![(
                    "description",
                    group
                        .and_then(|g| g.description.clone())
                        .unwrap_or_default(),
                )],
            ),
            GroupOp::Delete => Dialog::confirm(
                title("Delete "),
                Line::from(vec![
                    Span::raw("Delete "),
                    Span::styled(at.clone(), theme::style(theme::Role::Group)),
                    Span::raw(
                        "? Its members are told, then it is archived: its history stays readable, but it takes no more messages or members.",
                    ),
                ]),
            ),
            GroupOp::Add => Dialog::form(
                title("Add agents to "),
                "Agent names, separated by spaces or commas.",
                vec![("agents", String::new())],
            ),
            GroupOp::Remove => Dialog::form(
                title("Remove an agent from "),
                "The agent leaves the group.",
                vec![("agent", String::new())],
            ),
            GroupOp::Grant => Dialog::form(
                title("Grant write on "),
                "The agent may then broadcast to the group.",
                vec![("agent", String::new())],
            ),
            GroupOp::Revoke => Dialog::form(
                title("Revoke write on "),
                "The agent may then only read the group.",
                vec![("agent", String::new())],
            ),
        };
        let dialog = dialog
            .field_role("name", theme::Role::Group)
            .field_role("agents", theme::Role::Agent)
            .field_role("agent", theme::Role::Agent);
        self.dialog = Some((op, name, dialog));
    }

    /// A key while a dialog is open; `None` when none is (or for Ctrl-C).
    pub fn key(&mut self, id: u64, key: KeyEvent) -> Option<Transition> {
        let (op, group, dialog) = self.dialog.as_mut()?;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return None;
        }
        let values = match dialog.key(key) {
            Outcome::Open => return Some(Transition::Stay),
            Outcome::Cancel => {
                self.dialog = None;
                return Some(Transition::Stay);
            }
            Outcome::Submit(values) => values,
        };
        let value = |i: usize| {
            values
                .get(i)
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        let group = group.clone();
        let agent = value(0);
        let op = match op {
            GroupOp::Create => Admin::Create(proto::GroupRegisterRequest {
                group: value(0),
                description: Some(value(1)).filter(|d| !d.is_empty()),
            }),
            GroupOp::Describe => Admin::Describe(proto::GroupDescribeRequest {
                group,
                description: Some(value(0)).filter(|d| !d.is_empty()),
            }),
            GroupOp::Delete => Admin::Delete(proto::GroupRequest { group }),
            GroupOp::Add => Admin::Add(proto::GroupAddRequest {
                group,
                agents: value(0)
                    .split([' ', ','])
                    .filter(|a| !a.is_empty())
                    .map(String::from)
                    .collect(),
            }),
            GroupOp::Remove => Admin::Remove(proto::GroupRemoveRequest { group, agent }),
            GroupOp::Grant | GroupOp::Revoke => Admin::Writer(proto::SetGroupWriterRequest {
                group,
                agent,
                grant: *op == GroupOp::Grant,
            }),
        };
        self.dialog = None;
        self.notice = Some(Ok("Working…".into()));
        Some(Transition::Request(Request::Admin { id, op }))
    }

    pub fn on_data(&mut self, id: u64, data: &Data) {
        if let Data::Admin { id: to, result } = data
            && *to == id
        {
            self.notice = Some(result.clone());
        }
    }

    /// The last change's outcome, for the screen to show.
    pub fn notice(&self) -> Option<Line<'static>> {
        Some(match self.notice.as_ref()? {
            Ok(done) => Line::from(
                done.0
                    .iter()
                    .map(|(text, role)| Span::styled(text.clone(), theme::style(*role)))
                    .collect::<Vec<_>>(),
            ),
            Err(e) => Line::styled(e.clone(), theme::error()),
        })
    }

    /// Draw the open dialog over `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some((_, _, dialog)) = &self.dialog {
            dialog.render(frame, area);
        }
    }
}
