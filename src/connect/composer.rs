//! The composer: a text box with emacs keys that grows with its content.
//! `Enter` sends; `Alt-Enter` or `Ctrl-j` starts a new line.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType};
use ratatui_textarea::{TextArea, WrapMode};

use super::theme;

/// Text lines when empty; the box grows from here.
const MIN_LINES: u16 = 2;

pub struct Composer {
    text: TextArea<'static>,
    /// Who it sends to: an agent's name or `@group`.
    to: String,
    /// A send's id and submitted text; later edits remain if it succeeds.
    sending: Option<(u64, String)>,
    /// Why the last send failed.
    error: Option<String>,
}

impl Composer {
    pub fn new(to: &str) -> Composer {
        Composer {
            text: blank(),
            to: to.to_string(),
            sending: None,
            error: None,
        }
    }

    pub fn to(&self) -> &str {
        &self.to
    }

    /// Apply a key; the body to send when `Enter` sends one.
    pub fn key(&mut self, key: KeyEvent, send_id: u64) -> Option<String> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if alt => self.text.insert_newline(),
            KeyCode::Char('j') if ctrl => self.text.insert_newline(),
            // readline's unix-line-discard: delete back to the start of the
            // line, or join onto the previous line at its start.
            KeyCode::Char('u') if ctrl => {
                self.text.delete_line_by_head();
            }
            // Emacs undo. Terminals send Ctrl-_ (and Ctrl-/) as 0x1F, which
            // crossterm reports as Ctrl-7; with keyboard enhancement it is
            // the key itself.
            KeyCode::Char('_' | '/' | '7') if ctrl => {
                self.text.undo();
            }
            KeyCode::Enter => {
                let body = self.text.lines().join("\n");
                if self.sending.is_some() || body.trim().is_empty() {
                    return None;
                }
                self.sending = Some((send_id, body.clone()));
                self.error = None;
                return Some(body);
            }
            _ => {
                self.text.input(key);
            }
        }
        None
    }

    /// Consume a matching send result; success clears only unchanged text.
    pub fn sent(&mut self, id: u64, result: Result<(), String>) -> bool {
        if self.sending.as_ref().map(|(pending, _)| *pending) != Some(id) {
            return false;
        }
        let (_, body) = self.sending.take().expect("the matching send exists");
        match result {
            Ok(()) => {
                if self.text.lines().join("\n") == body {
                    self.text = blank();
                }
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        true
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, agent: bool) {
        let title = match (&self.error, self.sending.is_some(), focused) {
            (Some(e), ..) => format!(" Not sent: {e} "),
            (None, true, _) => " Sending… ".to_string(),
            (None, false, true) if agent => format!(
                " To {} · Enter sends · Ctrl-U/PgUp scroll · Esc back ",
                self.to
            ),
            (None, false, false) if agent => format!(" Navigation · Esc/i write to {} ", self.to),
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
            (None, true) => theme::style(theme::Role::Text),
            (None, false) => theme::dim(),
        };
        self.text.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .style(theme::style(theme::Role::HumanSurface))
                .border_style(border)
                .title(title),
        );
        let cursor = if focused {
            theme::style(theme::Role::Cursor)
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
    text.set_style(theme::style(theme::Role::HumanSurface));
    text.set_selection_style(theme::style(theme::Role::Selection));
    text.set_placeholder_style(theme::style(theme::Role::Muted));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_and_padding_share_the_human_surface() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        use ratatui::{Terminal, backend::TestBackend};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut composer = Composer::new("Mateo");
                typed(
                    &mut composer,
                    "a wrapped input with enough text to fill several lines",
                );
                for focused in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
                    terminal
                        .draw(|frame| {
                            frame.render_widget(Block::new().style(theme::base()), frame.area());
                            composer.render(frame, frame.area(), focused, true);
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    assert_buffer(buffer);
                    let surface = theme::style(theme::Role::HumanSurface)
                        .bg
                        .or(theme::base().bg)
                        .unwrap();
                    for y in 1..7 {
                        for x in 1..23 {
                            let cell = &buffer[(x, y)];
                            assert!(
                                cell.bg == surface
                                    || (focused
                                        && cell.bg
                                            == theme::style(theme::Role::Cursor)
                                                .bg
                                                .unwrap_or(surface))
                            );
                        }
                    }
                }
            });
        }
    }

    fn press(composer: &mut Composer, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        composer.key(KeyEvent::new(code, modifiers), 1)
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
        c.sent(1, Err("no agent named Mateo".into()));
        assert!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE).is_some());
        c.sent(1, Ok(()));
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
    fn ctrl_u_deletes_back_to_the_line_start_and_ctrl_underscore_undoes() {
        let mut c = Composer::new("Mateo");
        typed(&mut c, "one");
        press(&mut c, KeyCode::Char('j'), KeyModifiers::CONTROL);
        typed(&mut c, "two three");
        // Mid-line: only the text before the cursor goes.
        for _ in 0.."three".len() {
            press(&mut c, KeyCode::Char('b'), KeyModifiers::CONTROL);
        }
        press(&mut c, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["one", "three"]);
        // At column 0: the line joins onto the previous one.
        press(&mut c, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["onethree"]);
        assert_eq!(c.text.cursor(), (0, 3));
        for code in [KeyCode::Char('7'), KeyCode::Char('_'), KeyCode::Char('/')] {
            typed(&mut c, "x");
            press(&mut c, code, KeyModifiers::CONTROL);
            assert_eq!(c.text.lines(), ["onethree"], "{code:?}");
        }
        press(&mut c, KeyCode::Char('7'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["one", "three"]);
        press(&mut c, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["onethree"]);
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
