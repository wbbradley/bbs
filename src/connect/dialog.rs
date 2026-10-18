//! A modal dialog over the current screen: a short form of one-line fields,
//! or a yes/no question.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::theme;

const WIDTH: u16 = 60;

/// Pasted text for a one-line input: each line break or other control
/// character becomes a space.
pub fn one_line(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub struct Dialog {
    title: Line<'static>,
    /// Shown above the fields: what the dialog does.
    message: Line<'static>,
    fields: Vec<Field>,
    focus: usize,
    choices: Option<Choices>,
}

struct Choices {
    items: Vec<(String, String)>,
    selected: Option<usize>,
}

struct Field {
    label: &'static str,
    value: String,
    role: theme::Role,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Open,
    Cancel,
    /// The fields' values, in order; empty for a question.
    Submit(Vec<String>),
}

impl Dialog {
    /// A form: `Enter` submits, `Tab` and the arrows move between fields.
    pub fn form(
        title: impl Into<Line<'static>>,
        message: impl Into<Line<'static>>,
        fields: Vec<(&'static str, String)>,
    ) -> Dialog {
        Dialog {
            title: title.into(),
            message: message.into(),
            fields: fields
                .into_iter()
                .map(|(label, value)| Field {
                    label,
                    value,
                    role: theme::Role::Text,
                })
                .collect(),
            focus: 0,
            choices: None,
        }
    }

    /// Explicit choices are (opaque ID, display label), with no default selection.
    pub fn choose(
        title: impl Into<Line<'static>>,
        message: impl Into<Line<'static>>,
        items: Vec<(String, String)>,
    ) -> Self {
        let mut dialog = Self::form(title, message, Vec::new());
        dialog.choices = Some(Choices {
            items,
            selected: None,
        });
        dialog
    }

    /// Preserve a selection by ID, never by an index into a changed report.
    pub fn set_choices(&mut self, items: Vec<(String, String)>) {
        if let Some(choices) = &mut self.choices {
            let selected = choices
                .selected
                .and_then(|i| choices.items.get(i))
                .map(|(id, _)| id.clone());
            choices.selected =
                selected.and_then(|id| items.iter().position(|(candidate, _)| *candidate == id));
            choices.items = items;
        }
    }

    /// A yes/no question: `y` or `Enter` answers yes.
    pub fn confirm(title: impl Into<Line<'static>>, message: impl Into<Line<'static>>) -> Dialog {
        Dialog::form(title, message, Vec::new())
    }

    /// The form's typed field purpose, never inferred from its text.
    pub fn field_role(mut self, label: &str, role: theme::Role) -> Self {
        for field in &mut self.fields {
            if field.label == label {
                field.role = role;
            }
        }
        self
    }

    /// Append a paste to the focused field as one line; choices and
    /// questions ignore it, so pasted text never answers them.
    pub fn paste(&mut self, text: &str) {
        if self.choices.is_some() {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.push_str(&one_line(text));
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            return Outcome::Cancel;
        }
        if let Some(choices) = &mut self.choices {
            match key.code {
                KeyCode::Down | KeyCode::Tab if !choices.items.is_empty() => {
                    choices.selected = Some(
                        choices
                            .selected
                            .map_or(0, |i| (i + 1).min(choices.items.len() - 1)),
                    );
                }
                KeyCode::Up | KeyCode::BackTab if !choices.items.is_empty() => {
                    choices.selected = Some(
                        choices
                            .selected
                            .map_or(choices.items.len() - 1, |i| i.saturating_sub(1)),
                    );
                }
                KeyCode::Enter => {
                    if let Some((id, _)) = choices.selected.and_then(|i| choices.items.get(i)) {
                        return Outcome::Submit(vec![id.clone()]);
                    }
                }
                _ => {}
            }
            return Outcome::Open;
        }
        if key.code == KeyCode::Enter {
            return Outcome::Submit(self.fields.iter().map(|f| f.value.clone()).collect());
        }
        let Some(field) = self.fields.get_mut(self.focus) else {
            return match key.code {
                KeyCode::Char('y' | 'Y') => Outcome::Submit(Vec::new()),
                KeyCode::Char('n' | 'N' | 'q') => Outcome::Cancel,
                _ => Outcome::Open,
            };
        };
        match key.code {
            KeyCode::Char('u') if ctrl => field.value.clear(),
            KeyCode::Char(c) if !ctrl => field.value.push(c),
            KeyCode::Backspace => {
                field.value.pop();
            }
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % self.fields.len(),
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let hint = if self.choices.is_some() {
            "↑/↓ select · Enter apply · Esc cancel"
        } else if self.fields.is_empty() {
            "y yes · n no"
        } else if self.fields.len() > 1 {
            "Enter OK · Tab next field · Esc cancel"
        } else {
            "Enter OK · Esc cancel"
        };
        let mut message = self.message.clone();
        if self.choices.is_some() {
            for span in &mut message.spans {
                span.content = span.content.replace(['\n', '\r'], " ").into();
            }
            message = theme::clip_line(message, usize::from(WIDTH - 2));
        }
        let mut lines = vec![message, Line::raw("")];
        if let Some(choices) = &self.choices {
            // The choice explanation is one display row; keep the action hint
            // visible independently of a provider's label/reason length.
            let visible = usize::from(area.height).saturating_sub(6).max(1);
            let start = choices.selected.unwrap_or(0).saturating_sub(visible - 1);
            for (i, (_, label)) in choices.items.iter().enumerate().skip(start).take(visible) {
                let selected = choices.selected == Some(i);
                let line = Line::styled(
                    format!("{} {label}", if selected { "›" } else { " " }),
                    theme::style(if selected {
                        theme::Role::Selection
                    } else {
                        theme::Role::Text
                    }),
                );
                lines.push(theme::clip_line(line, usize::from(WIDTH - 2)));
            }
        }
        for (i, field) in self.fields.iter().enumerate() {
            let focused = i == self.focus;
            let mut value = Span::styled(field.value.clone(), theme::style(field.role));
            if focused {
                value = value.add_modifier(Modifier::UNDERLINED);
            }
            let mut spans = vec![
                Span::styled(format!("{:>12}: ", field.label), theme::heading()),
                value,
            ];
            if focused {
                spans.push(Span::styled("█", theme::dim()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(hint, theme::dim()));
        let inner_width = WIDTH - 2;
        let height: u16 = lines
            .iter()
            .map(|l| (l.width() as u16).div_ceil(inner_width).max(1))
            .sum::<u16>()
            + 2;
        let [row] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(area);
        let [rect] = Layout::horizontal([Constraint::Length(WIDTH)])
            .flex(Flex::Center)
            .areas(row);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(theme::style(theme::Role::Hotkey))
            .title({
                let mut spans = vec![Span::raw(" ")];
                spans.extend(self.title.spans.iter().cloned());
                spans.push(Span::raw(" "));
                Line::from(spans).style(theme::heading())
            })
            .style(theme::base());
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(dialog: &mut Dialog, code: KeyCode) -> Outcome {
        dialog.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn choices_require_explicit_selection_and_return_the_opaque_id() {
        let mut d = Dialog::choose(
            "Effort",
            "Current: provider default",
            vec![
                ("value_low".into(), "Low".into()),
                ("value_high".into(), "High".into()),
            ],
        );
        assert_eq!(press(&mut d, KeyCode::Enter), Outcome::Open);
        assert_eq!(press(&mut d, KeyCode::Char('m')), Outcome::Open);
        assert_eq!(press(&mut d, KeyCode::Enter), Outcome::Open);
        press(&mut d, KeyCode::Down);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            Outcome::Submit(vec!["value_low".into()])
        );
        press(&mut d, KeyCode::Down);
        press(&mut d, KeyCode::Down);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            Outcome::Submit(vec!["value_high".into()])
        );
        assert_eq!(press(&mut d, KeyCode::Esc), Outcome::Cancel);
    }

    #[test]
    fn changed_choices_preserve_selection_by_id_or_clear_it() {
        let mut d = Dialog::choose(
            "Effort",
            "Current: unknown",
            vec![("a".into(), "Low".into()), ("b".into(), "High".into())],
        );
        press(&mut d, KeyCode::Up);
        d.set_choices(vec![
            ("b".into(), "Updated label".into()),
            ("a".into(), "Low".into()),
        ]);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            Outcome::Submit(vec!["b".into()])
        );
        d.set_choices(vec![("a".into(), "Low".into())]);
        assert_eq!(press(&mut d, KeyCode::Enter), Outcome::Open);
        d.set_choices(Vec::new());
        press(&mut d, KeyCode::Down);
        press(&mut d, KeyCode::Up);
        assert_eq!(press(&mut d, KeyCode::Enter), Outcome::Open);
    }

    #[test]
    fn choices_keep_selection_and_cancel_hint_visible_at_minimum_size_in_all_schemes() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        use ratatui::{Terminal, backend::TestBackend};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut d = Dialog::choose(
                    "Effort",
                    format!(
                        "Current: unknown\n{}",
                        "provider default has no ordinal rank ".repeat(60)
                    ),
                    (0..20)
                        .map(|i| (format!("id{i}"), format!("choice{i} {}", "👩‍💻é".repeat(30))))
                        .collect(),
                );
                press(&mut d, KeyCode::Up);
                let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                terminal
                    .draw(|f| {
                        f.render_widget(Block::new().style(theme::base()), f.area());
                        d.render(f, Rect::new(0, 1, 80, 22));
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert_buffer(buffer);
                let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
                assert!(
                    text.contains("choice19")
                        && text.contains("Enter apply")
                        && text.contains("Esc cancel"),
                    "{text}"
                );
                assert!(
                    !text.contains("choice0 "),
                    "offscreen choices are not painted"
                );
                assert_eq!(
                    press(&mut d, KeyCode::Enter),
                    Outcome::Submit(vec!["id19".into()])
                );
            });
        }
    }

    #[test]
    fn forms_edit_each_field_and_submit_all() {
        let mut d = Dialog::form(
            "Create",
            "A new group.",
            vec![("name", String::new()), ("description", "x".into())],
        );
        for c in "crew".chars() {
            assert_eq!(press(&mut d, KeyCode::Char(c)), Outcome::Open);
        }
        press(&mut d, KeyCode::Tab);
        press(&mut d, KeyCode::Backspace);
        for c in "builds".chars() {
            press(&mut d, KeyCode::Char(c));
        }
        press(&mut d, KeyCode::Up);
        d.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "ops".chars() {
            press(&mut d, KeyCode::Char(c));
        }
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            Outcome::Submit(vec!["ops".into(), "builds".into()])
        );
        assert_eq!(press(&mut d, KeyCode::Esc), Outcome::Cancel);
    }

    #[test]
    fn questions_take_y_or_n() {
        let mut d = Dialog::confirm("Delete", "Delete @crew?");
        assert_eq!(press(&mut d, KeyCode::Char('x')), Outcome::Open);
        assert_eq!(press(&mut d, KeyCode::Char('n')), Outcome::Cancel);
        assert_eq!(
            press(&mut d, KeyCode::Char('y')),
            Outcome::Submit(Vec::new())
        );
    }

    #[test]
    fn pastes_fill_one_field_on_one_line_and_never_answer() {
        let mut d = Dialog::form(
            "Add",
            "",
            vec![("Agent", String::new()), ("Note", String::new())],
        );
        d.paste("Ma\r\nteo\n");
        press(&mut d, KeyCode::Tab);
        d.paste("a\tb");
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            Outcome::Submit(vec!["Ma teo ".into(), "a b".into()])
        );
        let mut q = Dialog::confirm("Delete", "");
        q.paste("y\n");
        assert_eq!(press(&mut q, KeyCode::Char('x')), Outcome::Open);
        let mut c = Dialog::choose("Effort", "", vec![("low".into(), "low".into())]);
        c.paste("low\n");
        assert_eq!(press(&mut c, KeyCode::Enter), Outcome::Open);
    }
}
