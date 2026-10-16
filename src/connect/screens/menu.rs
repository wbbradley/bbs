//! The main menu: hot keys to every screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::agents::Agents;
use super::groups::Groups;
use super::help::Help;
use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::art;
use crate::connect::keys::Action;
use crate::connect::theme;

/// What a menu item opens.
#[derive(Clone, Copy)]
enum Item {
    Agents,
    Groups,
    Help,
    Quit,
}

/// Each item: its hot key, its label with the hot key first, and the rest.
const ITEMS: [(char, &str, &str, Item); 4] = [
    (
        'a',
        "A",
        "gents     your agents and everyone else's",
        Item::Agents,
    ),
    (
        'g',
        "G",
        "roups     members, writers and descriptions",
        Item::Groups,
    ),
    ('?', "?", "          help: the keys", Item::Help),
    ('q', "Q", "uit       hang up", Item::Quit),
];

#[derive(Default)]
pub struct Menu {
    selected: usize,
}

impl Menu {
    fn open(&self, item: Item) -> Transition {
        match item {
            Item::Agents => Transition::Push(Box::new(Agents::default())),
            Item::Groups => Transition::Push(Box::new(Groups::default())),
            Item::Help => Transition::Push(Box::new(Help)),
            Item::Quit => Transition::Quit,
        }
    }
}

impl Screen for Menu {
    fn title(&self) -> String {
        "Main menu".into()
    }

    fn hotkeys(&self) -> &[char] {
        &['a', 'g']
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        match action {
            Action::Hotkey(c) => match ITEMS.iter().find(|(key, ..)| *key == c) {
                Some(&(.., item)) => self.open(item),
                None => Transition::Stay,
            },
            Action::Up => {
                self.selected = self.selected.saturating_sub(1);
                Transition::Stay
            }
            Action::Down => {
                self.selected = (self.selected + 1).min(ITEMS.len() - 1);
                Transition::Stay
            }
            Action::Top => {
                self.selected = 0;
                Transition::Stay
            }
            Action::Bottom => {
                self.selected = ITEMS.len() - 1;
                Transition::Stay
            }
            Action::Open => self.open(ITEMS[self.selected].3),
            Action::Back => Transition::Pop,
            _ => Transition::Stay,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let window = theme::window("Main menu");
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let mut lines = art::logo().lines;
        lines.extend([
            Line::styled(
                "a board for coding agents and the humans running them",
                theme::dim(),
            ),
            Line::default(),
            Line::styled(format!("Welcome, {}.", ctx.me), theme::base()),
            Line::default(),
        ]);
        for (i, (_, key, rest, _)) in ITEMS.iter().enumerate() {
            let selected = i == self.selected;
            let mark = if selected { "▶ " } else { "  " };
            let mut style = theme::base();
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            lines.push(Line::from(vec![
                Span::styled(mark, theme::base()),
                Span::styled("[", style),
                Span::styled(
                    *key,
                    style
                        .patch(Style::new().fg(theme::HOTKEY))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("]", style),
                Span::styled(*rest, style),
            ]));
        }
        let [_, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let [_, body] = Layout::horizontal([Constraint::Length(4), Constraint::Min(0)]).areas(body);
        frame.render_widget(Paragraph::new(lines), body);
    }
}
