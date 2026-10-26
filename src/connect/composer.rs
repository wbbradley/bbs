//! The composer: a text box with emacs keys that grows with its content.
//! `Enter` sends; `Alt-Enter` or `Ctrl-j` starts a new line.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::{Block, Paragraph};
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};

mod layout;
use layout::{Layout, Point};

use super::theme;

/// Text lines when empty; the box grows from here.
const MIN_LINES: u16 = 1;

#[derive(Clone)]
pub struct Composer {
    text: TextArea<'static>,
    /// Who it sends to: an agent's name or `@group`.
    to: String,
    /// A send's id and submitted text; later edits remain if it succeeds.
    sending: Option<(u64, String)>,
    /// Why the last send failed.
    error: Option<String>,
    area: Rect,
    /// Viewport row in display coordinates, including the uneditable padding row.
    top: usize,
}

impl Composer {
    pub fn new(to: &str) -> Composer {
        Composer {
            text: blank(),
            to: to.to_string(),
            sending: None,
            error: None,
            area: Rect::default(),
            top: 0,
        }
    }

    pub fn draft(&self) -> super::drafts::Draft {
        let cursor = self.text.cursor();
        super::drafts::Draft {
            text: self.text.lines().join("\n"),
            cursor: (cursor.0, cursor.1),
        }
    }

    /// Restore text and a logical cursor; pending sends and undo are session-only.
    pub fn restore(&mut self, draft: super::drafts::Draft) {
        self.text = TextArea::new(draft.text.split('\n').map(str::to_string).collect());
        self.text.set_wrap_mode(WrapMode::None);
        self.sending = None;
        self.error = None;
        self.top = 0;
        self.jump(DataCursor(draft.cursor.0, draft.cursor.1));
    }

    pub fn to(&self) -> &str {
        &self.to
    }

    /// Apply a key; the body to send when `Enter` sends one.
    pub fn key(&mut self, key: KeyEvent, send_id: u64) -> Option<String> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.navigate(key) {
            return None;
        }
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

    /// Insert a bracketed paste as one edit: its newlines are text, never a
    /// send. Terminals often paste line breaks as CR, so CRLF and CR become
    /// LF; other control characters but tab are dropped.
    pub fn paste(&mut self, text: &str) {
        let text: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|&c| matches!(c, '\n' | '\t') || !c.is_control())
            .collect();
        self.text.insert_str(text);
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

    fn layout(&self, width: u16) -> Layout {
        Layout::new(
            self.text.lines(),
            width,
            self.text.cursor(),
            self.text.tab_length(),
        )
    }

    /// One text row and scrollable top padding, plus dark and bottom separators.
    pub fn height(&self, width: u16, max_lines: u16) -> u16 {
        let lines = u16::try_from(self.layout(text_width(width)).height().saturating_add(1))
            .unwrap_or(u16::MAX);
        lines
            .clamp(MIN_LINES + 1, max_lines.max(MIN_LINES + 1))
            .saturating_add(2)
    }

    /// Whether the cursor is on the first display row of the whole draft, as
    /// last drawn; before any draw, on its first line.
    pub fn on_first_row(&self) -> bool {
        if self.area.width == 0 {
            return self.text.cursor().0 == 0;
        }
        self.layout(self.area.width).point(self.text.cursor()).row == 0
    }

    /// Only movements defined in display rows use the local map. Logical edits,
    /// selection, history and word/paragraph movements remain in TextArea.
    fn navigate(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.area.height.max(1) as usize;
        let movement = match key.code {
            KeyCode::Up if !ctrl && !alt => Some((false, 1, false)),
            KeyCode::Down if !ctrl && !alt => Some((true, 1, false)),
            KeyCode::Char('p') if ctrl && !alt => Some((false, 1, false)),
            KeyCode::Char('n') if ctrl && !alt => Some((true, 1, false)),
            KeyCode::PageUp => Some((false, page, true)),
            KeyCode::PageDown => Some((true, page, true)),
            KeyCode::Char('v') if alt && !ctrl => Some((false, page, true)),
            KeyCode::Char('v') if ctrl && !alt => Some((true, page, true)),
            _ => None,
        };
        let Some((down, amount, paging)) = movement else {
            return false;
        };
        if self.area.width == 0 {
            return false;
        }
        let layout = self.layout(self.area.width);
        let from = layout.point(self.text.cursor());
        let row = if paging {
            self.top = if down {
                self.top
                    .saturating_add(amount)
                    .min(layout.last_row().saturating_add(1))
            } else {
                self.top.saturating_sub(amount)
            };
            from.row
                .saturating_add(1)
                .clamp(self.top, self.top.saturating_add(page - 1))
                .max(1)
                .saturating_sub(1)
                .min(layout.last_row())
        } else if down {
            from.row.saturating_add(amount).min(layout.last_row())
        } else {
            from.row.saturating_sub(amount)
        };
        if from.row == row && !paging {
            return true;
        }
        let target = layout.source(Point { row, col: from.col });
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            if !self.text.is_selecting() {
                self.text.start_selection();
            }
        } else {
            self.text.cancel_selection();
        }
        self.jump(target);
        true
    }

    /// Jump's public coordinates are u16; finish larger targets with public
    /// movements so oversized drafts never silently wrap their cursor position.
    fn jump(&mut self, target: DataCursor) {
        let small = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        self.text
            .move_cursor(CursorMove::Jump(small(target.0), small(target.1)));
        // Each step is bounded and checked: a native movement can be inert at
        // a grapheme boundary even when its logical character offset differs.
        for _ in self.text.cursor().0..target.0 {
            let before = self.text.cursor();
            self.text.move_cursor(CursorMove::Down);
            if self.text.cursor().0 <= before.0 {
                return;
            }
        }
        if self.text.cursor().0 != target.0 {
            return;
        }
        if self.text.cursor().1 < target.1 {
            // End addresses the exact logical boundary, unlike Forward from a
            // u16-clamped offset inside a trailing combining grapheme.
            self.text.move_cursor(CursorMove::End);
        }
        for _ in target.1..self.text.cursor().1 {
            if self.text.cursor().1 <= target.1 {
                break;
            }
            let before = self.text.cursor();
            self.text.move_cursor(CursorMove::Back);
            let after = self.text.cursor();
            if after.0 != target.0 || after.1 >= before.1 {
                return;
            }
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The whole draft.
    pub fn text(&self) -> String {
        self.text.lines().join("\n")
    }

    pub fn sending(&self) -> bool {
        self.sending.is_some()
    }

    /// Clear the draft if it is still exactly `text`: what was submitted,
    /// with nothing typed since.
    pub fn clear_if(&mut self, text: &str) -> bool {
        if self.text.lines().join("\n") != text {
            return false;
        }
        self.text = blank();
        true
    }

    /// Whether the whole draft is the local command `command`; if so it is
    /// consumed, never sent.
    pub fn take_command(&mut self, command: &str) -> bool {
        if self.text.lines().join("\n").trim() != command {
            return false;
        }
        self.text = blank();
        true
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let surface = theme::style(theme::Role::HumanSurface);
        let (input, text) = draw_box(frame, area);
        let gutter = text.x - input.x;
        self.area = text;
        let layout = self.layout(self.area.width);
        let caret = layout.point(self.text.cursor()).row.saturating_add(1);
        let height = usize::from(self.area.height);
        if caret == 1 && height > 1 {
            self.top = 0;
        } else if caret < self.top {
            self.top = caret;
        } else if caret >= self.top.saturating_add(height) {
            self.top = caret.saturating_add(1).saturating_sub(height);
        }
        self.top = self
            .top
            .min(layout.height().saturating_add(1).saturating_sub(height));
        let padding = u16::from(self.top == 0).min(self.area.height);
        let text_area = Rect {
            y: self.area.y + padding,
            height: self.area.height - padding,
            ..self.area
        };
        if self.top <= 1 && text_area.height > 0 && gutter > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled("›", theme::style(theme::Role::Muted))).style(surface),
                Rect::new(input.x, text_area.y, gutter, 1),
            );
        }
        let text = layout.text(
            self.text.cursor(),
            focused,
            self.text.selection_range(),
            self.top.saturating_sub(1),
            usize::from(text_area.height),
        );
        frame.render_widget(Paragraph::new(text).style(surface), text_area);
    }
}

/// The box: a screen-coloured gap row, then the human surface. Returns the
/// surface and, inside it, the text area: past the gutter, above the
/// bottom pad.
fn draw_box(frame: &mut Frame, area: Rect) -> (Rect, Rect) {
    frame.render_widget(Block::new().style(theme::style(theme::Role::Screen)), area);
    let gap = area.height.min(1);
    let input = Rect {
        y: area.y + gap,
        height: area.height - gap,
        ..area
    };
    frame.render_widget(
        Block::new().style(theme::style(theme::Role::HumanSurface)),
        input,
    );
    let gutter = gutter_width(input.width);
    // Keep a caret row when a tiny rectangle cannot fit the bottom pad too.
    let bottom = u16::from(input.height > 1);
    let text = Rect {
        x: input.x + gutter,
        width: text_width(input.width),
        height: input.height - bottom,
        ..input
    };
    (input, text)
}

/// The columns a draft wraps to in a box `width` wide: past the gutter and
/// short of the right edge, as every thread's text is. The surface still
/// fills the box.
fn text_width(width: u16) -> u16 {
    crate::ui::thread::wrap_width(width - gutter_width(width))
}

/// Preserve one source column on narrow displays.
fn gutter_width(width: u16) -> u16 {
    width.saturating_sub(1).min(2)
}

fn blank() -> TextArea<'static> {
    let mut text = TextArea::default();
    text.set_wrap_mode(WrapMode::None);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_draft_preserves_raw_unicode_and_cursor_without_pending_send() {
        let mut original = Composer::new("Mateo");
        original.restore(super::super::drafts::Draft {
            text: "  界é\nsecond\n".into(),
            cursor: (1, 3),
        });
        let snapshot = original.draft();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(original.key(enter, 1), Some(snapshot.text.clone()));
        let mut restored = Composer::new("Mateo");
        restored.restore(original.draft());
        assert_eq!(restored.draft(), snapshot);
        restored.key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE), 2);
        assert_eq!(restored.key(enter, 3).as_deref(), Some("  界é\nsecXond\n"));
        restored.restore(super::super::drafts::Draft {
            text: "short".into(),
            cursor: (usize::MAX, usize::MAX),
        });
        assert!(restored.draft().cursor.0 == 0);
        assert!(restored.draft().cursor.1 <= 5);
    }

    #[test]
    fn spacer_uses_screen_surface_while_input_uses_human_surface() {
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
                            composer.render(frame, frame.area(), focused);
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    assert_buffer(buffer);
                    assert!((0..24).all(|x| buffer[(x, 0)].symbol() == " "));
                    let screen = theme::style(theme::Role::Screen);
                    for x in 0..24 {
                        assert_eq!(buffer[(x, 0)].fg, screen.fg.unwrap());
                        assert_eq!(buffer[(x, 0)].bg, screen.bg.unwrap());
                    }
                    let first: String = (0..24).map(|x| buffer[(x, 2)].symbol()).collect();
                    assert!(first.starts_with("› a wrapped input"));
                    // The text stops one column short of the box's edge.
                    assert_eq!(composer.area, Rect::new(2, 1, 21, 6));
                    assert_eq!(
                        composer.text.lines(),
                        ["a wrapped input with enough text to fill several lines"]
                    );
                    assert!(
                        !buffer
                            .content
                            .iter()
                            .any(|cell| "╭╮╰╯│─".contains(cell.symbol()))
                    );
                    let surface = theme::style(theme::Role::HumanSurface)
                        .bg
                        .or(theme::base().bg)
                        .unwrap();
                    for y in 1..8 {
                        for x in 0..24 {
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

    #[test]
    fn hintless_input_keeps_full_width_draft_and_caret_in_every_scheme() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for width in [1, 2, 8, 80] {
                    for to in ["Mateo", "@crew"] {
                        let mut c = Composer::new(to);
                        c.restore(super::super::drafts::Draft {
                            text: "  é👩‍💻\nsecond".into(),
                            cursor: (0, 3),
                        });
                        let before = c.draft();
                        let buffer = draw(&mut c, width, 5);
                        assert_buffer(&buffer);
                        assert_eq!(
                            c.area,
                            Rect::new(gutter_width(width), 1, text_width(width), 3)
                        );
                        assert_eq!(c.draft(), before);
                        let caret = c.layout(c.area.width).point(c.text.cursor());
                        let cursor = theme::base()
                            .patch(theme::style(theme::Role::HumanSurface))
                            .patch(theme::style(theme::Role::Cursor));
                        let cell = &buffer[(
                            c.area.x + caret.col as u16,
                            1 + (caret.row + 1 - c.top) as u16,
                        )];
                        assert_eq!(cell.fg, cursor.fg.unwrap());
                        assert_eq!(cell.bg, cursor.bg.unwrap());
                        let all: String = buffer.content.iter().map(|c| c.symbol()).collect();
                        assert!(!all.contains(to) && !all.contains("Enter sends"));
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
    fn paste_inserts_line_breaks_as_text_in_one_undoable_edit() {
        let mut c = Composer::new("Mateo");
        typed(&mut c, "ab");
        press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        c.paste("x\r\ny\rz\u{1b}\tq");
        assert_eq!(c.text.lines(), ["ax", "y", "z\tqb"]);
        assert_eq!(c.text.cursor(), (2, 3));
        press(&mut c, KeyCode::Char('7'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["ab"]);
        c.paste("one\ntwo");
        let body = press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(body.as_deref(), Some("aone\ntwob"));
        // A paste while sending is an edit and sends nothing, so success
        // keeps the text, as does failure.
        c.paste("!\r\u{3}\t");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), None);
        c.sent(1, Ok(()));
        assert_eq!(c.text.lines(), ["aone", "two!", "\tb"]);
        let body = press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(body.as_deref(), Some("aone\ntwo!\n\tb"));
        c.paste("more");
        c.sent(1, Err("offline".into()));
        assert_eq!(c.text.lines(), ["aone", "two!", "\tmoreb"]);
        assert_eq!(c.error.as_deref(), Some("offline"));
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

    fn draw(c: &mut Composer, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| c.render(frame, frame.area(), true))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn drafts_stop_one_column_short_of_the_box_edge_with_the_caret_in_view() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for width in [60u16, 80] {
                    // Drafts that once filled the text area exactly.
                    let old = usize::from(width - gutter_width(width));
                    for draft in ["x".repeat(old), "界".repeat(old / 2)] {
                        let mut c = Composer::new("Mateo");
                        c.text.insert_str(&draft);
                        let height = c.height(width, 6);
                        let buffer = draw(&mut c, width, height);
                        assert_buffer(&buffer);
                        let surface = theme::base().patch(theme::style(theme::Role::HumanSurface));
                        for y in 1..height {
                            let edge = &buffer[(width - 1, y)];
                            assert_eq!(edge.symbol(), " ", "{scheme:?} {width} row {y}: {draft}");
                            assert_eq!(edge.bg, surface.bg.unwrap(), "{scheme:?}");
                            assert_ne!(
                                unicode_width::UnicodeWidthStr::width(
                                    buffer[(width - 2, y)].symbol()
                                ),
                                2,
                                "{scheme:?} {width} row {y}"
                            );
                        }
                        // Nothing lost, and the caret at the end in view.
                        assert_eq!(c.text.lines().join(""), draft);
                        let caret = c.layout(c.area.width).point(c.text.cursor());
                        assert!(
                            caret.row + 1 >= c.top
                                && caret.row + 1 < c.top + usize::from(c.area.height)
                        );
                        assert!(caret.col < usize::from(c.area.width) || caret.col == 0);
                    }
                }
                // Too narrow for a margin: drawn, never a panic.
                for width in [1u16, 2, 3] {
                    let mut c = Composer::new("Mateo");
                    c.text.insert_str("abc");
                    draw(&mut c, width, 4);
                }
            });
        }
    }

    #[test]
    fn visual_navigation_and_shift_selection_use_the_painted_rows() {
        let mut c = Composer::new("Mateo");
        c.text.insert_str("abcd  efgh ij");
        c.jump(DataCursor(0, 3));
        // Four text columns: past the gutter, short of the margin.
        draw(&mut c, 7, 5);
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 9));
        press(&mut c, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(c.text.cursor(), (0, 13));
        press(&mut c, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(c.text.cursor(), (0, 8));
        press(&mut c, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(c.text.selection_range(), Some(((0, 2), (0, 8))));
        let buffer = draw(&mut c, 7, 5);
        let selected =
            theme::style(theme::Role::HumanSurface).patch(theme::style(theme::Role::Selection));
        assert_eq!(buffer[(5, 2)].fg, selected.fg.unwrap());
        assert_eq!(buffer[(5, 2)].bg, selected.bg.unwrap());
        assert!(buffer[(5, 2)].modifier.contains(selected.add_modifier));
        c.text.copy();
        assert_eq!(c.text.yank_text(), "cd  ef");
        // Native yank/undo still operate on exact source, including hidden spaces.
        press(&mut c, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["abcd  efcd  efgh ij"]);
        press(&mut c, KeyCode::Char('7'), KeyModifiers::CONTROL);
        assert_eq!(c.text.lines(), ["abcd  efgh ij"]);
    }

    #[test]
    fn layout_resize_paging_and_hidden_carets_keep_the_source_and_cursor_visible() {
        let mut c = Composer::new("Mateo");
        let source = "abcd  efgh ijkl mnop qrst\n          indented\n\t你é👩‍💻";
        c.text.insert_str(source);
        for width in [4, 1, 8, 2, 24] {
            c.jump(DataCursor(0, 5)); // hidden soft separator at width four
            let buffer = draw(&mut c, width, 4);
            assert_eq!(c.text.lines().join("\n"), source);
            assert_eq!(c.text.cursor(), (0, 5));
            let caret = c.layout(c.area.width).point(c.text.cursor());
            assert!(caret.row + 1 >= c.top && caret.row + 1 < c.top + usize::from(c.area.height));
            let cell = &buffer[(
                c.area.x + caret.col as u16,
                (caret.row + 1 - c.top + usize::from(c.area.y)) as u16,
            )];
            let cursor =
                theme::style(theme::Role::HumanSurface).patch(theme::style(theme::Role::Cursor));
            assert_eq!(cell.fg, cursor.fg.unwrap());
            assert_eq!(cell.bg, cursor.bg.unwrap());
            press(&mut c, KeyCode::PageDown, KeyModifiers::NONE);
            let after_page = c.layout(c.area.width).point(c.text.cursor());
            assert!(after_page.row >= caret.row);
            press(&mut c, KeyCode::Char('v'), KeyModifiers::ALT);
            assert!(c.layout(c.area.width).point(c.text.cursor()).row <= after_page.row);
        }
        assert_eq!(
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE).as_deref(),
            Some(source)
        );
        typed(&mut c, "X");
        let edited = c.text.lines().join("\n");
        c.sent(1, Ok(()));
        assert_eq!(
            c.text.lines().join("\n"),
            edited,
            "pending success retains later edits"
        );
    }

    #[test]
    fn exact_width_end_caret_is_reachable_in_both_directions() {
        let mut c = Composer::new("Mateo");
        c.text.insert_str("abcd");
        draw(&mut c, 7, 5);
        assert_eq!(c.layout(4).point(c.text.cursor()), Point { row: 1, col: 0 });
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 0));
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 4));
        c.text = TextArea::new(vec!["abcd".into(), "  x".into()]);
        c.jump(DataCursor(0, 4));
        let layout = c.layout(4);
        assert_eq!(
            layout.height(),
            2,
            "a hard newline needs no extra blank row"
        );
        assert_eq!(layout.point(c.text.cursor()), Point { row: 1, col: 0 });
    }

    #[test]
    fn large_combining_tail_moves_to_its_logical_end_without_forward_looping() {
        let mut c = Composer::new("Mateo");
        c.text = TextArea::new(vec![format!("{}\u{301}", "x".repeat(65_535))]);
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            c.text.cancel_selection();
            c.jump(DataCursor(0, 65_527));
            draw(&mut c, 11, 5);
            let layout = c.layout(8);
            let from = layout.point(c.text.cursor());
            assert_eq!(
                layout.source(Point {
                    row: from.row + 1,
                    col: from.col
                }),
                DataCursor(0, 65_536)
            );
            // Every movement in jump has a finite range and a no-progress guard.
            press(&mut c, KeyCode::Down, modifiers);
            assert_eq!(c.text.cursor(), (0, 65_536));
            assert_eq!(
                c.text.selection_range(),
                if modifiers.is_empty() {
                    None
                } else {
                    Some(((0, 65_527), (0, 65_536)))
                }
            );
            assert_eq!(c.text.lines()[0].chars().count(), 65_536);
        }
    }

    #[test]
    fn large_jump_stops_before_combining_and_zwj_suffixes() {
        let mut c = Composer::new("Mateo");
        let prefix = "x".repeat(65_536);
        let suffix = "e\u{301}👩‍💻z\u{301}🇺🇸";
        c.text = TextArea::new(vec![format!("{prefix}{suffix}")]);
        c.text.move_cursor(CursorMove::End);
        c.text.start_selection();
        let end = c.text.cursor().1;
        c.jump(DataCursor(0, 65_536));
        assert_eq!(c.text.cursor(), (0, 65_536));
        assert_eq!(c.text.selection_range(), Some(((0, 65_536), (0, end))));
        c.text.copy();
        assert_eq!(c.text.yank_text(), suffix);
        assert_eq!(c.text.lines()[0], format!("{prefix}{suffix}"));
    }

    #[test]
    fn visual_jumps_do_not_truncate_large_source_coordinates() {
        let mut c = Composer::new("Mateo");
        c.text = TextArea::new(vec!["x".repeat(65_540)]);
        c.jump(DataCursor(0, 65_539));
        assert_eq!(c.text.cursor(), (0, 65_539));
        // No silent u16 truncation when a visual move lands past column 65535.
        draw(&mut c, 11, 5);
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 65_531));
        c.text = TextArea::new(vec![String::new(); 65_538]);
        c.jump(DataCursor(65_537, 0));
        assert_eq!(c.text.cursor(), (65_537, 0));
    }

    #[test]
    fn top_padding_scrolls_away_and_returns_without_a_source_position() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut c = Composer::new("Mateo");
                c.restore(super::super::drafts::Draft {
                    text: "one\ntwo\nthree\nfour".into(),
                    cursor: (0, 0),
                });
                let source = c.draft();
                let buffer = draw(&mut c, 10, 5);
                let surface = theme::base().patch(theme::style(theme::Role::HumanSurface));
                for x in 0..8 {
                    assert_eq!(buffer[(x, 1)].symbol(), " ");
                    assert_eq!(buffer[(x, 1)].fg, surface.fg.unwrap());
                    assert_eq!(buffer[(x, 1)].bg, surface.bg.unwrap());
                }
                assert_eq!(buffer[(2, 2)].symbol(), "o");
                // Moving or selecting up at the first source caret never enters padding.
                for (code, modifiers) in [
                    (KeyCode::Up, KeyModifiers::NONE),
                    (KeyCode::Up, KeyModifiers::SHIFT),
                    (KeyCode::Char('p'), KeyModifiers::CONTROL),
                    (KeyCode::PageUp, KeyModifiers::NONE),
                ] {
                    press(&mut c, code, modifiers);
                    draw(&mut c, 10, 5);
                    assert_eq!(c.draft(), source);
                    assert_eq!(c.top, 0);
                }
                c.jump(DataCursor(3, 0));
                let buffer = draw(&mut c, 10, 5);
                assert_eq!(c.top, 2);
                assert_eq!(
                    buffer[(2, 1)].symbol(),
                    "t",
                    "text occupies old padding row"
                );
                for _ in 0..3 {
                    press(&mut c, KeyCode::Up, KeyModifiers::NONE);
                    draw(&mut c, 10, 5);
                }
                assert_eq!(c.draft(), source);
                assert_eq!(
                    c.top, 0,
                    "returning to first visual text row restores padding"
                );
                let buffer = draw(&mut c, 10, 5);
                assert_eq!(buffer[(0, 1)].symbol(), " ");
                press(&mut c, KeyCode::Down, KeyModifiers::SHIFT);
                assert_eq!(c.text.selection_range(), Some(((0, 0), (1, 0))));
                c.text.copy();
                assert_eq!(c.text.yank_text(), "one\n");
                let buffer = draw(&mut c, 8, 5);
                for x in 0..8 {
                    assert_eq!(buffer[(x, 1)].bg, surface.bg.unwrap());
                }
            });
        }
    }

    #[test]
    fn padding_wrap_resize_and_paging_keep_caret_visible_and_draft_bytes_unchanged() {
        let mut c = Composer::new("@crew");
        let source = "abcd  é👩‍💻\n         intentional\n\tend";
        c.restore(super::super::drafts::Draft {
            text: source.into(),
            cursor: (0, 0),
        });
        for width in [1, 2, 4, 8, 30] {
            for height in [2, 3, 4, 6] {
                for (code, modifiers) in [
                    (KeyCode::PageDown, KeyModifiers::NONE),
                    (KeyCode::PageUp, KeyModifiers::NONE),
                    (KeyCode::Down, KeyModifiers::NONE),
                    (KeyCode::Char('v'), KeyModifiers::CONTROL),
                    (KeyCode::Char('v'), KeyModifiers::ALT),
                ] {
                    draw(&mut c, width, height);
                    press(&mut c, code, modifiers);
                    let before = c.draft();
                    let buffer = draw(&mut c, width, height);
                    assert_eq!(c.draft(), before);
                    assert_eq!(c.draft().text, source);
                    let caret = c.layout(c.area.width).point(c.text.cursor());
                    let row = caret.row + 1;
                    assert!(row >= c.top && row < c.top + usize::from(c.area.height));
                    let cell =
                        &buffer[(c.area.x + caret.col as u16, c.area.y + (row - c.top) as u16)];
                    let style = theme::base()
                        .patch(theme::style(theme::Role::HumanSurface))
                        .patch(theme::style(theme::Role::Cursor));
                    assert_eq!(cell.fg, style.fg.unwrap());
                    assert_eq!(cell.bg, style.bg.unwrap());
                }
            }
        }
        assert_eq!(
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE).as_deref(),
            Some(source)
        );
    }

    #[test]
    fn gutter_and_bottom_padding_are_display_only_and_never_selected() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut c = Composer::new("Mateo");
                c.restore(super::super::drafts::Draft {
                    text: "abcdef ghij\n  indented".into(),
                    cursor: (0, 0),
                });
                let snapshot = c.draft();
                let buffer = draw(&mut c, 9, 8);
                assert_buffer(&buffer);
                let line = |y| (0..9).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                // Six text columns, and the box's last column left clear.
                assert_eq!(line(2), "› abcdef ");
                assert!(line(3).starts_with("  ghij"));
                let surface = theme::base().patch(theme::style(theme::Role::HumanSurface));
                assert_eq!(line(7), "         ");
                for x in 0..9 {
                    let cell = &buffer[(x, 7)];
                    assert_eq!(cell.fg, surface.fg.unwrap());
                    assert_eq!(cell.bg, surface.bg.unwrap());
                    assert_eq!(cell.modifier, surface.add_modifier);
                }
                assert_eq!(c.draft(), snapshot);
                press(&mut c, KeyCode::Down, KeyModifiers::SHIFT);
                let selected = c.text.selection_range().unwrap();
                let buffer = draw(&mut c, 9, 8);
                assert_eq!(c.text.selection_range(), Some(selected));
                for y in 1..8 {
                    for x in 0..2 {
                        let cell = &buffer[(x, y)];
                        assert_eq!(cell.bg, surface.bg.unwrap());
                        assert!(!cell.modifier.contains(ratatui::style::Modifier::REVERSED));
                    }
                }
                c.text.copy();
                assert_eq!(c.text.yank_text(), "abcdef ");
                c.text.cancel_selection();
                c.text.move_cursor(CursorMove::Bottom);
                c.text.move_cursor(CursorMove::End);
                let buffer = draw(&mut c, 8, 5);
                assert!(c.top > 1, "first source row scrolled out");
                assert!((0..5).all(|y| buffer[(0, y)].symbol() != "›"));
                assert!((0..8).all(|x| buffer[(x, 4)].symbol() == " "));
                assert_eq!(c.draft().text, snapshot.text);
            });
        }
    }

    #[test]
    fn initial_three_light_rows_grow_to_four_on_the_second_wrapped_row() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut c = Composer::new("Mateo");
                for (source, light_rows) in [("", 3), ("abc", 3), ("abc defg", 4)] {
                    c.restore(super::super::drafts::Draft {
                        text: source.into(),
                        cursor: (0, source.chars().count()),
                    });
                    let snapshot = c.draft();
                    let height = c.height(8, 20);
                    assert_eq!(height, light_rows + 1);
                    let buffer = draw(&mut c, 8, height);
                    let surface = theme::base().patch(theme::style(theme::Role::HumanSurface));
                    let cursor = surface.patch(theme::style(theme::Role::Cursor));
                    for x in 0..8 {
                        for y in [1, height - 1] {
                            let cell = &buffer[(x, y)];
                            assert_eq!(cell.symbol(), " ");
                            assert_eq!(cell.fg, surface.fg.unwrap());
                            assert_eq!(cell.bg, surface.bg.unwrap());
                        }
                    }
                    assert_eq!(buffer[(0, 2)].symbol(), "›");
                    let caret = c.layout(c.area.width).point(c.text.cursor());
                    let cell = &buffer[(c.area.x + caret.col as u16, 2 + caret.row as u16)];
                    assert_eq!(cell.fg, cursor.fg.unwrap());
                    assert_eq!(cell.bg, cursor.bg.unwrap());
                    assert_eq!(c.draft(), snapshot);
                }
            });
        }
    }

    #[test]
    fn the_box_grows_with_wrapped_text_up_to_its_limit() {
        let mut c = Composer::new("Mateo");
        assert_eq!(c.height(22, 4), 4);
        typed(&mut c, &"x".repeat(60));
        // Three source rows plus top padding fill the four-row viewport cap.
        assert_eq!(c.height(22, 4), 6);
        typed(&mut c, &"x".repeat(60));
        assert_eq!(c.height(22, 4), 6);
        let cursor = c.text.cursor();
        let height = c.height(22, 4);
        draw(&mut c, 22, height);
        assert_eq!(
            c.area,
            Rect::new(2, 1, 19, 4),
            "outer padding preserves four viewport rows"
        );
        assert_eq!(c.text.cursor(), cursor);
        assert!(c.layout(c.area.width).point(cursor).row < c.top + 4);
        // Even a one-row limit retains a source row and the display padding.
        assert_eq!(c.height(22, 1), 4);
    }
}
