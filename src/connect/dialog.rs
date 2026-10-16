//! A modal dialog over the current screen: a short form of one-line fields,
//! or a yes/no question.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::theme;

const WIDTH: u16 = 60;

pub struct Dialog {
    title: String,
    /// Shown above the fields: what the dialog does.
    message: String,
    fields: Vec<Field>,
    focus: usize,
}

struct Field {
    label: &'static str,
    value: String,
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
    pub fn form(title: &str, message: &str, fields: Vec<(&'static str, String)>) -> Dialog {
        Dialog {
            title: title.to_string(),
            message: message.to_string(),
            fields: fields
                .into_iter()
                .map(|(label, value)| Field { label, value })
                .collect(),
            focus: 0,
        }
    }

    /// A yes/no question: `y` or `Enter` answers yes.
    pub fn confirm(title: &str, message: &str) -> Dialog {
        Dialog::form(title, message, Vec::new())
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            return Outcome::Cancel;
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
        let hint = if self.fields.is_empty() {
            "y yes · n no"
        } else if self.fields.len() > 1 {
            "Enter OK · Tab next field · Esc cancel"
        } else {
            "Enter OK · Esc cancel"
        };
        let mut lines = vec![Line::raw(self.message.clone()), Line::raw("")];
        for (i, field) in self.fields.iter().enumerate() {
            let focused = i == self.focus;
            let mut value = Span::styled(field.value.clone(), theme::base().fg(Color::White));
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
            .border_style(theme::base().fg(theme::HOTKEY))
            .title(Span::styled(format!(" {} ", self.title), theme::heading()))
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
}
