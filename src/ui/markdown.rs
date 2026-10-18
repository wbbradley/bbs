//! Markdown uses the same semantic roles as messages and activity.
use super::theme::{self, Role};
use ratatui::{style::Style, text::Text};
use tui_markdown::{AlertKind, Options, StyleSheet};

#[derive(Clone)]
struct Styles;
impl StyleSheet for Styles {
    fn heading(&self, _level: u8) -> Style {
        theme::style(Role::Heading)
    }
    fn code(&self) -> Style {
        theme::style(Role::Technical)
    }
    fn link(&self) -> Style {
        theme::style(Role::Accent).underlined()
    }
    fn blockquote(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn heading_meta(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn metadata_block(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn html(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn math_inline(&self) -> Style {
        theme::style(Role::Technical).italic()
    }
    fn math_display(&self) -> Style {
        theme::style(Role::Technical)
    }
    fn footnote_ref(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn footnote_def(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn definition_term(&self) -> Style {
        theme::style(Role::Heading)
    }
    fn definition_description(&self) -> Style {
        theme::style(Role::Text)
    }
    fn table_header(&self) -> Style {
        theme::style(Role::Heading)
    }
    fn table_cell(&self) -> Style {
        Style::new()
    }
    fn table_border(&self) -> Style {
        theme::style(Role::Muted)
    }
    fn list_marker(&self) -> Style {
        theme::style(Role::Accent)
    }
    fn image_alt(&self) -> Style {
        theme::style(Role::Muted).italic()
    }
    fn alert(&self, kind: AlertKind) -> Style {
        theme::style(match kind {
            AlertKind::Note | AlertKind::Important => Role::Accent,
            AlertKind::Tip => Role::Success,
            AlertKind::Warning => Role::Warning,
            AlertKind::Caution => Role::Error,
        })
    }
}
pub fn render(source: &str) -> Text<'_> {
    let mut text = tui_markdown::from_str_with_options(source, &Options::new(Styles));
    // Set ordinary text on each line too: callers may consume lines directly.
    for line in &mut text.lines {
        line.style = theme::style(Role::Text).patch(line.style);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{activity::Activity, message::message_lines, theme::testing::*};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        widgets::{Block, Paragraph},
    };
    #[test]
    fn markdown_and_activity_share_roles_without_library_colors() {
        let source = "# Heading\n\n**strong** and *emphasis* and ~~deleted~~ `inline` [link](https://example.com)\n\n> quote\n\n> [!WARNING]\n> warning\n\n- list\n\n| header |\n| --- |\n| cell |\n\n```rust\nlet x = 1;\n```\n";
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let row = crate::proto::Message {
                    body: source.into(),
                    ..Default::default()
                };
                let mut lines = message_lines(&row, false, Some("strong"));
                let mut activity = Activity::new(true);
                lines.extend(
                    activity
                        .render(&serde_json::json!({"event":"text", "text":source}).to_string()),
                );
                for event in [
                    "warning",
                    "error",
                    "turn_end",
                    "thought",
                    "permission",
                    "prompt",
                ] {
                    lines.extend(activity.render(&serde_json::json!({"event": event, "text":"detail", "message":"detail"}).to_string()));
                }
                let code = lines
                    .iter()
                    .find(|line| line.to_string().contains("let x = 1;"))
                    .unwrap();
                assert_eq!(code.style.fg, theme::style(Role::Technical).fg);
                let plain = crate::ui::to_ansi(&lines, false);
                assert!(!plain.contains('\x1b'));
                assert!(plain.contains("https://example.com"));
                let mut terminal = Terminal::new(TestBackend::new(100, 100)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            Block::new().style(theme::style(Role::Screen)),
                            frame.area(),
                        );
                        frame.render_widget(Paragraph::new(lines), frame.area());
                    })
                    .unwrap();
                assert_buffer(terminal.backend().buffer());
            });
        }
    }
}
