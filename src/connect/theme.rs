//! Shared-theme layout helpers for the connect screens.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// The smallest terminal the layout is drawn for.
pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;

pub use crate::ui::theme::{Role, on_surface, style};

pub fn base() -> Style {
    style(Role::Screen)
}
pub fn dim() -> Style {
    style(Role::Muted)
}
pub fn heading() -> Style {
    style(Role::Heading)
}
pub fn error() -> Style {
    style(Role::Error)
}

/// A screen's heading row above its content area; screens have no outer
/// border (dialogs keep their own).
pub fn screen(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let [title_row, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {title} "), heading()))),
        title_row,
    );
    body
}

/// The bar across the top: board decoration plus a structured screen title.
pub fn title_bar(screen: Line<'static>) -> Line<'static> {
    let shade = style(Role::Logo);
    let bar = style(Role::TitleBar);
    let mut spans = vec![Span::styled("░▒▓█", shade), Span::styled(" BBS · ", bar)];
    spans.extend(screen.spans.into_iter().map(|mut span| {
        span.style = on_surface(bar, screen.style.patch(span.style));
        span
    }));
    spans.extend([Span::styled(" ", bar), Span::styled("█▓▒░", shade)]);
    Line::from(spans).alignment(Alignment::Center)
}

/// A typed heading for a thread; other screens keep the plain helper.
pub fn screen_line(frame: &mut Frame, area: Rect, title: Line<'static>) -> Rect {
    let [title_row, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let mut spans = vec![Span::styled(" ", heading())];
    spans.extend(title.spans.into_iter().map(|mut span| {
        span.style = heading().patch(title.style).patch(span.style);
        span
    }));
    frame.render_widget(Paragraph::new(Line::from(spans)), title_row);
    body
}

/// Truncate an already-styled line without flattening category spans.
pub fn clip_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if line.width() <= width {
        return line;
    }
    let mut used = 0;
    let mut spans = Vec::new();
    'outer: for span in line.spans {
        let mut text = String::new();
        for part in span.content.graphemes(true) {
            if used + part.width() >= width {
                if !text.is_empty() {
                    spans.push(Span::styled(text, span.style));
                }
                break 'outer;
            }
            used += part.width();
            text.push_str(part);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    if width > 0 {
        spans.push(Span::raw("…"));
    }
    line.spans = spans;
    line
}

/// The bar across the bottom: server, human and time online.
pub fn status_bar(server: &str, human: &str, online: Duration) -> Line<'static> {
    let secs = online.as_secs();
    let clock = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    let bar = style(Role::StatusBar);
    Line::from(vec![
        Span::styled(format!(" {server} "), bar.add_modifier(Modifier::BOLD)),
        Span::styled("│", bar),
        Span::styled(format!(" {human} "), on_surface(bar, style(Role::Human))),
        Span::styled("│", bar),
        Span::raw(" online "),
        Span::styled(format!("{clock} "), on_surface(bar, style(Role::Timestamp))),
    ])
    .style(bar)
}

/// The title bar, the screen's area, and the status bar.
pub fn frame_areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area)
}

pub fn too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// A centred notice in place of a layout that would not fit.
pub fn render_too_small(frame: &mut Frame) {
    let area = frame.area();
    let text = vec![
        Line::styled("terminal too small", heading()),
        Line::styled(
            format!(
                "{}×{}; needs {MIN_WIDTH}×{MIN_HEIGHT}",
                area.width, area.height
            ),
            dim(),
        ),
    ];
    let [middle] = Layout::vertical([Constraint::Length(2)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    frame.render_widget(Block::new().style(base()), area);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
}
