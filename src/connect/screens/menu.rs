//! The main menu: hot keys to every screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::agents::Agents;
use super::blockers::BlockersPage;
use super::config::Config;
use super::groups::Groups;
use super::help::Help;
use super::thread::Thread;
use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::art;
use crate::connect::keys::Action;
use crate::connect::theme;

/// What a menu item opens.
#[derive(Clone, Copy)]
enum Item {
    Agents,
    Groups,
    Blockers,
    Feed,
    Config,
    Help,
    Quit,
}

/// Each item: its hot key, its label with the hot key first, and the rest.
const ITEMS: [(char, &str, &str, Item); 7] = [
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
    (
        'b',
        "B",
        "lockers   the questions agents are waiting on",
        Item::Blockers,
    ),
    (
        'f',
        "F",
        "eed       every message, as bbs view shows it",
        Item::Feed,
    ),
    ('c', "C", "onfig     color scheme", Item::Config),
    ('?', "?", "          help: the keys", Item::Help),
    ('q', "Q", "uit       hang up", Item::Quit),
];

const HOTKEYS: [char; 5] = [ITEMS[0].0, ITEMS[1].0, ITEMS[2].0, ITEMS[3].0, ITEMS[4].0];

#[derive(Default)]
pub struct Menu {
    selected: usize,
}

impl Menu {
    fn open(&self, item: Item) -> Transition {
        match item {
            Item::Agents => Transition::Push(Box::new(Agents::default())),
            Item::Groups => Transition::Push(Box::new(Groups::new())),
            Item::Blockers => Transition::Push(Box::new(BlockersPage)),
            Item::Feed => Transition::Push(Box::new(Thread::feed())),
            Item::Config => Transition::Push(Box::new(Config::new())),
            Item::Help => Transition::Push(Box::new(Help::default())),
            Item::Quit => Transition::Quit,
        }
    }
}

impl Screen for Menu {
    fn title(&self) -> String {
        "Main menu".into()
    }

    /// The letter items; `?` and `q` already do the same in the keymap.
    fn hotkeys(&self) -> &[char] {
        &HOTKEYS
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

    /// The logo, version, welcome and items as one block, centred both ways.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let items: Vec<Line> = ITEMS
            .iter()
            .enumerate()
            .map(|(i, (_, key, rest, _))| {
                let selected = i == self.selected;
                let mut style = theme::base();
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(vec![
                    Span::styled(if selected { "▶ " } else { "  " }, theme::base()),
                    Span::styled("[", style),
                    Span::styled(
                        *key,
                        style
                            .patch(theme::style(theme::Role::Hotkey))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("]", style),
                    Span::styled(*rest, style),
                ])
            })
            .collect();
        // Pad the items to one width so their keys line up once centred.
        let column = items.iter().map(Line::width).max().unwrap_or(0);
        let items = items.into_iter().map(|mut line| {
            let pad = column - line.width();
            line.spans
                .push(Span::styled(" ".repeat(pad), theme::base()));
            line
        });
        let mut lines = art::logo().lines;
        lines.extend([
            Line::default(),
            Line::styled(
                format!("v{} · {}", crate::release::VERSION, ctx.server),
                theme::dim(),
            ),
            Line::styled(
                "a board for coding agents and the humans running them",
                theme::dim(),
            ),
            Line::default(),
            Line::from(vec![
                Span::raw("Welcome, "),
                Span::styled(ctx.me.clone(), theme::style(theme::Role::Human)),
                Span::raw("."),
            ]),
            Line::default(),
        ]);
        lines.extend(items);
        // Cut long names rather than wrap the block past the screen.
        let lines: Vec<Line> = lines
            .into_iter()
            .map(|line| theme::clip_line(line, usize::from(area.width)))
            .collect();
        let [middle] = Layout::vertical([Constraint::Length(lines.len() as u16)])
            .flex(Flex::Center)
            .areas(area);
        frame.render_widget(Block::new().style(theme::base()), area);
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), middle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_letter_item_is_a_hot_key() {
        for (key, ..) in ITEMS {
            assert!(HOTKEYS.contains(&key) || matches!(key, '?' | 'q'), "{key}");
        }
    }
}
