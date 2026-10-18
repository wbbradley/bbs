//! The composer: a text box with emacs keys that grows with its content.
//! `Enter` sends; `Alt-Enter` or `Ctrl-j` starts a new line.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};

mod layout;
use layout::{Layout, Point};

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
    area: Rect,
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

    /// Two text rows when empty, plus a blank separator and the hint row.
    pub fn height(&self, width: u16, max_lines: u16) -> u16 {
        let lines = u16::try_from(self.layout(width).height()).unwrap_or(u16::MAX);
        lines
            .clamp(MIN_LINES, max_lines.max(MIN_LINES))
            .saturating_add(2)
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
                self.top.saturating_add(amount).min(layout.last_row())
            } else {
                self.top.saturating_sub(amount)
            };
            from.row
                .clamp(self.top, self.top.saturating_add(page - 1))
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, agent: bool) {
        let target = || Span::styled(self.to.as_str(), theme::style(theme::Role::Target));
        let keys = |text| Span::styled(text, theme::style(theme::Role::KeyHint));
        let title = match (&self.error, self.sending.is_some(), focused) {
            (Some(e), ..) => Line::from(format!(" Not sent: {e} ")),
            (None, true, _) => Line::from(" Sending… "),
            (None, false, true) => Line::from(vec![
                Span::raw(" To "),
                target(),
                keys(if agent {
                    " · Enter sends · PgUp scroll · Esc back "
                } else {
                    " · Enter sends · Alt-Enter new line · Esc done "
                }),
            ]),
            (None, false, false) if agent => Line::from(vec![
                Span::raw(" Navigation · "),
                keys("Esc/i write to "),
                target(),
                Span::raw(" "),
            ]),
            (None, false, false) => {
                Line::from(vec![keys(" i: write to "), target(), Span::raw(" ")])
            }
        };
        let surface = theme::style(theme::Role::HumanSurface);
        let hint = match (&self.error, focused) {
            (Some(_), _) => theme::style(theme::Role::Error),
            (None, true) => theme::style(theme::Role::Text),
            (None, false) => theme::style(theme::Role::Muted),
        };
        frame.render_widget(Block::new().style(theme::style(theme::Role::Screen)), area);
        let gap = area.height.min(1);
        let input = Rect {
            y: area.y + gap,
            height: area.height - gap,
            ..area
        };
        let block = Block::new().style(surface).title(title).title_style(hint);
        self.area = block.inner(input);
        frame.render_widget(block, input);
        let layout = self.layout(self.area.width);
        let caret = layout.point(self.text.cursor());
        let height = usize::from(self.area.height);
        if caret.row < self.top {
            self.top = caret.row;
        } else if caret.row >= self.top + height {
            self.top = (caret.row + 1).saturating_sub(height);
        }
        self.top = self.top.min(layout.height().saturating_sub(height));
        let text = layout.text(
            self.text.cursor(),
            focused,
            self.text.selection_range(),
            self.top,
            height,
        );
        frame.render_widget(Paragraph::new(text).style(surface), self.area);
    }
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
                            composer.render(frame, frame.area(), focused, true);
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
                    let title: String = (0..24).map(|x| buffer[(x, 1)].symbol()).collect();
                    assert!(title.contains(if focused { "To Mateo" } else { "Navigation" }));
                    assert_eq!(composer.area, Rect::new(0, 2, 24, 6));
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
    fn title_targets_and_key_hints_use_semantic_styles_for_agents_and_groups() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        use ratatui::{Terminal, backend::TestBackend, style::Modifier};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for (to, agent) in [("Mateo", true), ("@crew", false)] {
                    for focused in [false, true] {
                        let mut composer = Composer::new(to);
                        let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
                        terminal
                            .draw(|frame| {
                                frame
                                    .render_widget(Block::new().style(theme::base()), frame.area());
                                composer.render(frame, frame.area(), focused, agent);
                            })
                            .unwrap();
                        let buffer = terminal.backend().buffer();
                        assert_buffer(buffer);
                        let title: String = (0..100).map(|x| buffer[(x, 1)].symbol()).collect();
                        let target_x = title[..title.find(to).unwrap()].chars().count() as u16;
                        let target = theme::base()
                            .patch(theme::style(theme::Role::HumanSurface))
                            .patch(theme::style(theme::Role::Target));
                        for x in target_x..target_x + to.len() as u16 {
                            assert_eq!(buffer[(x, 1)].fg, target.fg.unwrap());
                            assert_eq!(buffer[(x, 1)].bg, target.bg.unwrap());
                            assert!(!buffer[(x, 1)].modifier.contains(Modifier::ITALIC));
                        }
                        let keys_byte = title
                            .find(if focused {
                                "Enter"
                            } else if agent {
                                "Esc/i"
                            } else {
                                "i:"
                            })
                            .unwrap();
                        let keys_x = title[..keys_byte].chars().count() as u16;
                        let keys = theme::base()
                            .patch(theme::style(theme::Role::HumanSurface))
                            .patch(theme::style(theme::Role::KeyHint));
                        assert_eq!(buffer[(keys_x, 1)].fg, keys.fg.unwrap());
                        assert_eq!(buffer[(keys_x, 1)].bg, keys.bg.unwrap());
                        assert!(buffer[(keys_x, 1)].modifier.contains(Modifier::ITALIC));
                        if focused {
                            let prefix = title.find("To ").unwrap() as u16;
                            assert_eq!(
                                buffer[(prefix, 1)].fg,
                                theme::style(theme::Role::Text)
                                    .fg
                                    .or(theme::base().fg)
                                    .unwrap()
                            );
                            assert!(!buffer[(prefix, 1)].modifier.contains(Modifier::ITALIC));
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

    fn draw(c: &mut Composer, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| c.render(frame, frame.area(), true, true))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn visual_navigation_and_shift_selection_use_the_painted_rows() {
        let mut c = Composer::new("Mateo");
        c.text.insert_str("abcd  efgh ij");
        c.jump(DataCursor(0, 3));
        draw(&mut c, 4, 4);
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 9));
        press(&mut c, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(c.text.cursor(), (0, 13));
        press(&mut c, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(c.text.cursor(), (0, 8));
        press(&mut c, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(c.text.selection_range(), Some(((0, 2), (0, 8))));
        let buffer = draw(&mut c, 4, 4);
        let selected =
            theme::style(theme::Role::HumanSurface).patch(theme::style(theme::Role::Selection));
        assert_eq!(buffer[(3, 2)].fg, selected.fg.unwrap());
        assert_eq!(buffer[(3, 2)].bg, selected.bg.unwrap());
        assert!(buffer[(3, 2)].modifier.contains(selected.add_modifier));
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
            let caret = c.layout(width).point(c.text.cursor());
            assert!(caret.row >= c.top && caret.row < c.top + 2);
            let cell = &buffer[(caret.col as u16, (caret.row - c.top + 2) as u16)];
            let cursor =
                theme::style(theme::Role::HumanSurface).patch(theme::style(theme::Role::Cursor));
            assert_eq!(cell.fg, cursor.fg.unwrap());
            assert_eq!(cell.bg, cursor.bg.unwrap());
            press(&mut c, KeyCode::PageDown, KeyModifiers::NONE);
            let after_page = c.layout(width).point(c.text.cursor());
            assert!(after_page.row >= caret.row);
            press(&mut c, KeyCode::Char('v'), KeyModifiers::ALT);
            assert!(c.layout(width).point(c.text.cursor()).row <= after_page.row);
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
        draw(&mut c, 4, 4);
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
            draw(&mut c, 8, 4);
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
        draw(&mut c, 8, 4);
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text.cursor(), (0, 65_531));
        c.text = TextArea::new(vec![String::new(); 65_538]);
        c.jump(DataCursor(65_537, 0));
        assert_eq!(c.text.cursor(), (65_537, 0));
    }

    #[test]
    fn the_box_grows_with_wrapped_text_up_to_its_limit() {
        let mut c = Composer::new("Mateo");
        assert_eq!(c.height(22, 4), 4);
        typed(&mut c, &"x".repeat(60));
        // Full width is available: three text rows plus gap and hint.
        assert_eq!(c.height(22, 4), 5);
        typed(&mut c, &"x".repeat(60));
        assert_eq!(c.height(22, 4), 6);
        let cursor = c.text.cursor();
        let height = c.height(22, 4);
        draw(&mut c, 22, height);
        assert_eq!(
            c.area,
            Rect::new(0, 2, 22, 4),
            "gap and hint preserve four input rows"
        );
        assert_eq!(c.text.cursor(), cursor);
        assert!(c.layout(22).point(cursor).row < c.top + 4);
        // A limit under two lines still leaves two.
        assert_eq!(c.height(22, 1), 4);
    }
}
