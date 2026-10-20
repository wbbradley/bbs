//! The keys, generated from the keymap table.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Row, Table, TableState};

use super::navigate;

use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::keys::{Action, BINDINGS};
use crate::connect::theme;

const AGENT_KEYS: &[(&str, &str)] = &[
    (
        "Agent insert",
        "Enter sends; Esc back, saving the draft/cursor",
    ),
    ("PgUp", "agent insert: navigate the timeline"),
    (
        "PgDn",
        "agent navigation: follow and focus input at the bottom",
    ),
    ("Ctrl-U", "insert: delete to line start; join at column 0"),
    ("Esc", "agent navigation: return to insert mode"),
    (
        "Ctrl-T",
        "agents: compact (no tool calls or others' mail) / verbose",
    ),
    ("↑ / ↓", "agent insert: move within the draft"),
    (
        "Tab / Shift-Tab",
        "agents: increase / decrease effort; default opens picker",
    ),
];

/// The keys, one per row; it scrolls when they outgrow the window.
#[derive(Default)]
pub struct Help {
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Screen for Help {
    fn navigation_blocked(&self) -> bool {
        true
    }

    fn title(&self) -> String {
        "Help".into()
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        if navigate(
            &mut self.state,
            BINDINGS.len() + AGENT_KEYS.len() + 1,
            self.page,
            action,
        ) {
            return Transition::Stay;
        }
        match action {
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        let inner = theme::screen(frame, area, "Keys · j/k scroll");
        let mut rows: Vec<Row> = BINDINGS
            .iter()
            .map(|b| {
                let keys: Vec<String> = b.keys.iter().map(|k| k.label()).collect();
                Row::new(vec![
                    Line::styled(keys.join(" / "), theme::style(theme::Role::Hotkey)),
                    Line::raw(b.help),
                ])
            })
            .collect();
        rows.push(Row::new(vec![
            Line::styled("[X]", theme::style(theme::Role::Hotkey)),
            Line::raw("a menu item's hot key"),
        ]));
        rows.extend(AGENT_KEYS.iter().map(|(key, help)| {
            Row::new(vec![
                Line::styled(*key, theme::style(theme::Role::Hotkey)),
                Line::raw(*help),
            ])
        }));
        let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(10)])
            .style(theme::base())
            .row_highlight_style(theme::style(theme::Role::Selection));
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
    }
}
