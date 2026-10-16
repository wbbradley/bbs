//! Rendering shared by `bbs view`, `bbs headless logs` and `bbs connect`.

pub mod activity;
pub mod message;
pub mod thread;

use ratatui::backend::IntoCrossterm as _;
use ratatui::crossterm::style::ContentStyle;
use ratatui::style::Style;
use ratatui::text::Line;

/// Styled lines as text for a terminal, one per line; plain text when `color`
/// is false.
pub fn to_ansi(lines: &[Line], color: bool) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            let style = line.style.patch(span.style);
            if !color || span.content.is_empty() || style == Style::new() {
                out.push_str(&span.content);
            } else {
                let style: ContentStyle = style.into_crossterm();
                out.push_str(&style.apply(&*span.content).to_string());
            }
        }
        out.push('\n');
    }
    out
}
