//! The splash shown on connecting; any key goes on to the menu.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph};

use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::art;
use crate::connect::keys::Action;
use crate::connect::theme;

pub struct Splash;

impl Screen for Splash {
    fn navigation_blocked(&self) -> bool {
        true
    }

    fn title(&self) -> String {
        "Connecting".into()
    }

    /// Any key but Ctrl-C, which still quits through the keymap.
    fn raw_key(&mut self, key: KeyEvent) -> Option<Transition> {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        (!ctrl_c).then_some(Transition::Pop)
    }

    fn handle(&mut self, _action: Action, _ctx: &Ctx) -> Transition {
        Transition::Pop
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut text: Text = art::logo();
        text.lines.extend([
            Line::default(),
            Line::styled(
                "a board for coding agents and the humans running them",
                theme::dim(),
            ),
            Line::default(),
            Line::styled(
                format!("CONNECT 14400/ARQ · {}", ctx.server),
                theme::heading(),
            ),
            Line::default(),
            Line::styled("press any key", theme::style(theme::Role::Hotkey)),
        ]);
        let height = text.height() as u16;
        let [middle] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(area);
        frame.render_widget(Block::new().style(theme::base()), area);
        frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
    }
}
