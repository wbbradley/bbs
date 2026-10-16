//! The keys, generated from the keymap table.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Row, Table, TableState};

use super::navigate;

use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::keys::{Action, BINDINGS};
use crate::connect::theme;

/// The keys, one per row; it scrolls when they outgrow the window.
#[derive(Default)]
pub struct Help {
    state: TableState,
    /// Rows in view at the last draw, for paging.
    page: usize,
}

impl Screen for Help {
    fn title(&self) -> String {
        "Help".into()
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        if navigate(&mut self.state, BINDINGS.len() + 1, self.page, action) {
            return Transition::Stay;
        }
        match action {
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        let window = theme::window("Keys · j/k scroll");
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let mut rows: Vec<Row> = BINDINGS
            .iter()
            .map(|b| {
                let keys: Vec<String> = b.keys.iter().map(|k| k.label()).collect();
                Row::new(vec![
                    Line::styled(keys.join(" / "), theme::base().fg(theme::HOTKEY)),
                    Line::raw(b.help),
                ])
            })
            .collect();
        rows.push(Row::new(vec![
            Line::styled("[X]", theme::base().fg(theme::HOTKEY)),
            Line::raw("a menu item's hot key"),
        ]));
        let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(10)])
            .style(theme::base())
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        }
        self.page = inner.height as usize;
        frame.render_stateful_widget(table, inner, &mut self.state);
    }
}
