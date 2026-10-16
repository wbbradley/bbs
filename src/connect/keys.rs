//! Keys as named actions. One table drives both key handling and the help
//! screen, so the help cannot drift from the real keys.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Top,
    Bottom,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    Open,
    Back,
    Help,
    Reload,
    ToggleAll,
    Search,
    Compose,
    NextMatch,
    PrevMatch,
    /// A screen's own hot key, such as a menu item's.
    Hotkey(char),
    Quit,
}

/// One key, as the help screen shows it.
#[derive(Clone, Copy, Debug)]
pub enum Key {
    Char(char),
    Ctrl(char),
    /// The same character twice in a row, like `gg`.
    Twice(char),
    Code(KeyCode),
}

pub struct Binding {
    pub keys: &'static [Key],
    pub action: Action,
    pub help: &'static str,
}

pub const BINDINGS: &[Binding] = &[
    Binding {
        keys: &[Key::Char('j'), Key::Code(KeyCode::Down)],
        action: Action::Down,
        help: "down",
    },
    Binding {
        keys: &[Key::Char('k'), Key::Code(KeyCode::Up)],
        action: Action::Up,
        help: "up",
    },
    Binding {
        keys: &[Key::Twice('g'), Key::Code(KeyCode::Home)],
        action: Action::Top,
        help: "top",
    },
    Binding {
        keys: &[Key::Char('G'), Key::Code(KeyCode::End)],
        action: Action::Bottom,
        help: "bottom",
    },
    Binding {
        keys: &[Key::Ctrl('d')],
        action: Action::HalfPageDown,
        help: "half a page down",
    },
    Binding {
        keys: &[Key::Ctrl('u')],
        action: Action::HalfPageUp,
        help: "half a page up",
    },
    Binding {
        keys: &[Key::Code(KeyCode::PageDown)],
        action: Action::PageDown,
        help: "a page down",
    },
    Binding {
        keys: &[Key::Code(KeyCode::PageUp)],
        action: Action::PageUp,
        help: "a page up",
    },
    Binding {
        keys: &[Key::Code(KeyCode::Enter)],
        action: Action::Open,
        help: "open",
    },
    Binding {
        keys: &[Key::Char('/')],
        action: Action::Search,
        help: "threads: search",
    },
    Binding {
        keys: &[Key::Char('i'), Key::Code(KeyCode::Tab)],
        action: Action::Compose,
        help: "threads: write (Enter sends, Esc done)",
    },
    Binding {
        keys: &[Key::Char('n')],
        action: Action::NextMatch,
        help: "threads: next match",
    },
    Binding {
        keys: &[Key::Char('N')],
        action: Action::PrevMatch,
        help: "threads: previous match",
    },
    Binding {
        keys: &[Key::Char('q'), Key::Code(KeyCode::Esc)],
        action: Action::Back,
        help: "back (quit from the main menu)",
    },
    Binding {
        keys: &[Key::Char('?')],
        action: Action::Help,
        help: "this help",
    },
    Binding {
        keys: &[Key::Char('r')],
        action: Action::Reload,
        help: "reload",
    },
    Binding {
        keys: &[Key::Char('a')],
        action: Action::ToggleAll,
        help: "agents: yours or everyone's",
    },
    Binding {
        keys: &[Key::Ctrl('c')],
        action: Action::Quit,
        help: "quit",
    },
];

impl Key {
    pub fn label(&self) -> String {
        match self {
            Key::Char(c) => c.to_string(),
            Key::Ctrl(c) => format!("Ctrl-{c}"),
            Key::Twice(c) => format!("{c}{c}"),
            Key::Code(KeyCode::Up) => "↑".into(),
            Key::Code(KeyCode::Down) => "↓".into(),
            Key::Code(KeyCode::PageUp) => "PgUp".into(),
            Key::Code(KeyCode::PageDown) => "PgDn".into(),
            Key::Code(code) => code.to_string(),
        }
    }

    /// Whether `key` is this key (a `Twice` key matches each of its presses).
    fn matches(&self, key: &KeyEvent) -> bool {
        let plain = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        match *self {
            Key::Char(c) | Key::Twice(c) => plain && key.code == KeyCode::Char(c),
            Key::Ctrl(c) => {
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
            }
            Key::Code(code) => plain && key.code == code,
        }
    }
}

/// Turns key presses into actions, remembering the first half of `gg`.
#[derive(Default)]
pub struct Keymap {
    pending: Option<char>,
}

impl Keymap {
    /// The action for `key`. A screen's `hotkeys` come first; a key that
    /// starts a two-key binding waits for the second.
    pub fn resolve(&mut self, key: KeyEvent, hotkeys: &[char]) -> Option<Action> {
        let pending = self.pending.take();
        let twice = |k: &Key| matches!(k, Key::Twice(_));
        for binding in BINDINGS {
            for k in binding.keys.iter().filter(|k| twice(k) && k.matches(&key)) {
                let Key::Twice(c) = *k else { unreachable!() };
                if pending == Some(c) {
                    return Some(binding.action);
                }
            }
        }
        if let KeyCode::Char(c) = key.code
            && (key.modifiers - KeyModifiers::SHIFT).is_empty()
            && hotkeys.contains(&c.to_ascii_lowercase())
        {
            return Some(Action::Hotkey(c.to_ascii_lowercase()));
        }
        for binding in BINDINGS {
            match binding.keys.iter().find(|k| k.matches(&key)) {
                Some(Key::Twice(c)) => {
                    self.pending = Some(*c);
                    return None;
                }
                Some(_) => return Some(binding.action),
                None => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn events(key: &Key) -> Vec<KeyEvent> {
        match *key {
            Key::Char(c) => vec![press(KeyCode::Char(c), KeyModifiers::NONE)],
            Key::Ctrl(c) => vec![press(KeyCode::Char(c), KeyModifiers::CONTROL)],
            Key::Twice(c) => vec![press(KeyCode::Char(c), KeyModifiers::NONE); 2],
            Key::Code(code) => vec![press(code, KeyModifiers::NONE)],
        }
    }

    #[test]
    fn every_listed_key_does_what_the_help_says() {
        for binding in BINDINGS {
            for key in binding.keys {
                let mut keymap = Keymap::default();
                let actions: Vec<_> = events(key)
                    .into_iter()
                    .filter_map(|e| keymap.resolve(e, &[]))
                    .collect();
                assert_eq!(actions, [binding.action], "{}", key.label());
            }
        }
    }

    #[test]
    fn a_lone_g_is_dropped_and_the_next_key_still_counts() {
        let mut keymap = Keymap::default();
        let g = press(KeyCode::Char('g'), KeyModifiers::NONE);
        let j = press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(keymap.resolve(g, &[]), None);
        assert_eq!(keymap.resolve(j, &[]), Some(Action::Down));
        assert_eq!(keymap.resolve(g, &[]), None);
        assert_eq!(keymap.resolve(g, &[]), Some(Action::Top));
    }

    #[test]
    fn hot_keys_come_first() {
        let mut keymap = Keymap::default();
        let g = press(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(keymap.resolve(g, &['g']), Some(Action::Hotkey('g')));
        let q = press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(keymap.resolve(q, &['a']), Some(Action::Back));
        let ctrl_a = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(keymap.resolve(ctrl_a, &['a']), None);
    }
}
