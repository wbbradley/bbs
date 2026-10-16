//! The composer: a text box with emacs keys that grows with its content.
//! `Enter` sends; `Alt-Enter` or `Ctrl-j` starts a new line.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType};
use ratatui_textarea::{TextArea, WrapMode};

use super::theme;

/// Text lines when empty; the box grows from here.
const MIN_LINES: u16 = 2;

pub struct Composer {
    text: TextArea<'static>,
    /// Who it sends to: an agent's name or `@group`.
    to: String,
    /// A send is in flight; its text stays until it succeeds.
    sending: bool,
    /// Why the last send failed.
    error: Option<String>,
}

impl Composer {
    pub fn new(to: &str) -> Composer {
        Composer {
            text: blank(),
            to: to.to_string(),
            sending: false,
            error: None,
        }
    }

    pub fn to(&self) -> &str {
        &self.to
    }

    /// Apply a key; the body to send when `Enter` sends one.
    pub fn key(&mut self, key: KeyEvent) -> Option<String> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if alt => self.text.insert_newline(),
            KeyCode::Char('j') if ctrl => self.text.insert_newline(),
            KeyCode::Enter => {
                let body = self.text.lines().join("\n");
                if self.sending || body.trim().is_empty() {
                    return None;
                }
                self.sending = true;
                self.error = None;
                return Some(body);
            }
            _ => {
                self.text.input(key);
            }
        }
        None
    }

    /// The outcome of the send in flight: on success the box empties.
    pub fn sent(&mut self, result: Result<(), String>) {
        self.sending = false;
        match result {
            Ok(()) => {
                self.text = blank();
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Rows the box takes at `width`, borders included: two text lines when
    /// empty, growing with the wrapped text up to `max_lines`.
    pub fn height(&self, width: u16, max_lines: u16) -> u16 {
        let inner = width.saturating_sub(2).max(1) as usize;
        let lines: usize = self
            .text
            .lines()
            .iter()
            .map(|l| {
                unicode_width::UnicodeWidthStr::width(l.as_str())
                    .div_ceil(inner)
                    .max(1)
            })
            .sum();
        let lines = u16::try_from(lines).unwrap_or(u16::MAX);
        lines.clamp(MIN_LINES, max_lines.max(MIN_LINES)) + 2
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let title = match (&self.error, self.sending, focused) {
            (Some(e), ..) => format!(" Not sent: {e} "),
            (None, true, _) => " Sending… ".to_string(),
            (None, false, true) => {
                format!(
                    " To {} · Enter sends · Alt-Enter new line · Esc done ",
                    self.to
                )
            }
            (None, false, false) => format!(" i: write to {} ", self.to),
        };
        let border = match (&self.error, focused) {
            (Some(_), _) => theme::error(),
            (None, true) => theme::base(),
            (None, false) => theme::dim(),
        };
        self.text.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(title),
        );
        let cursor = if focused {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        self.text.set_cursor_style(cursor);
        frame.render_widget(&self.text, area);
    }
}

fn blank() -> TextArea<'static> {
    let mut text = TextArea::default();
    text.set_wrap_mode(WrapMode::WordOrGlyph);
    text.set_cursor_line_style(Style::new());
    text.set_style(theme::base());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(composer: &mut Composer, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        composer.key(KeyEvent::new(code, modifiers))
    }

    fn typed(composer: &mut Composer, text: &str) {
        for c in text.chars() {
            press(composer, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn enter_sends_and_alt_enter_or_ctrl_j_start_a_new_line() {
        let mut c = Composer::new("Mateo");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), None);
        typed(&mut c, "one");
        press(&mut c, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut c, "two");
        press(&mut c, KeyCode::Char('j'), KeyModifiers::CONTROL);
        typed(&mut c, "three");
        let body = press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(body.as_deref(), Some("one\ntwo\nthree"));
        // One send at a time; a failure keeps the text for another try.
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), None);
        c.sent(Err("no agent named Mateo".into()));
        assert!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE).is_some());
        c.sent(Ok(()));
        assert_eq!(c.text.lines(), [""]);
    }

    #[test]
    fn emacs_keys_edit() {
        let mut c = Composer::new("Mateo");
        typed(&mut c, "hello world");
        // Alt-f goes to the start of the next word.
        press(&mut c, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut c, KeyCode::Char('f'), KeyModifiers::ALT);
        press(&mut c, KeyCode::Char('k'), KeyModifiers::CONTROL);
        typed(&mut c, "there ");
        press(&mut c, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut c, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["hello there world"]);
        press(&mut c, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press(&mut c, KeyCode::Char('b'), KeyModifiers::ALT);
        press(&mut c, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["hello here "]);
    }

    #[test]
    fn the_box_grows_with_wrapped_text_up_to_its_limit() {
        let mut c = Composer::new("Mateo");
        assert_eq!(c.height(22, 4), 4);
        typed(&mut c, &"x".repeat(60));
        // 20 columns inside the borders: three lines.
        assert_eq!(c.height(22, 4), 5);
        typed(&mut c, &"x".repeat(60));
        assert_eq!(c.height(22, 4), 6);
        // A limit under two lines still leaves two.
        assert_eq!(c.height(22, 1), 4);
    }
}
