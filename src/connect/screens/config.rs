//! Local appearance settings, reached from the main menu.

use crate::connect::{
    app::{Ctx, Screen, Transition},
    keys::Action,
    theme,
};
use crate::ui::theme::{self as scheme, Role, Scheme};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, TableState},
};

pub struct Config {
    selection: TableState,
}

impl Config {
    pub fn new() -> Self {
        let mut selection = TableState::default();
        selection.select(Scheme::ALL.iter().position(|s| *s == scheme::current()));
        Self { selection }
    }
}

impl Screen for Config {
    fn title(&self) -> String {
        "Config".into()
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        if super::navigate(
            &mut self.selection,
            Scheme::ALL.len(),
            Scheme::ALL.len(),
            action,
        ) {
            return Transition::Stay;
        }
        match action {
            Action::Open => Transition::Theme(Scheme::ALL[self.selection.selected().unwrap_or(0)]),
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let window = theme::window("Config");
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let current = scheme::current();
        let mut lines = vec![
            Line::default(),
            Line::styled(" Color scheme", scheme::style(Role::Heading)),
            Line::styled(
                format!(" Current: {}", current.label()),
                scheme::style(Role::Text),
            ),
            Line::default(),
        ];
        for (i, choice) in Scheme::ALL.into_iter().enumerate() {
            let style = scheme::style(if self.selection.selected() == Some(i) {
                Role::Selection
            } else {
                Role::Text
            });
            lines.push(Line::from(Span::styled(
                format!(
                    " {} {}",
                    if choice == current { "[x]" } else { "[ ]" },
                    choice.label()
                ),
                style,
            )));
        }
        lines.extend([
            Line::default(),
            Line::styled(
                " ↑/↓ select · Enter apply and save · Esc back",
                scheme::style(Role::KeyHint),
            ),
            Line::default(),
            Line::styled(
                " Changes apply immediately and are saved on this device.",
                scheme::style(Role::Muted),
            ),
            Line::styled(
                " Startup --theme or BBS_THEME overrides the saved choice.",
                scheme::style(Role::Muted),
            ),
        ]);
        if let Some(error) = &ctx.preferences.error {
            lines.push(Line::default());
            lines.push(Line::styled(
                format!(" {error}"),
                scheme::style(Role::Error),
            ));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
