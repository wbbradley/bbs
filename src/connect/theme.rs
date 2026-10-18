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

pub use crate::ui::theme::{Role, style};

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

/// The bar across the top: the board's name and the current screen.
pub fn title_bar(screen: &str) -> Line<'static> {
    let shade = style(Role::Logo);
    Line::from(vec![
        Span::styled("░▒▓█", shade),
        Span::styled(format!(" BBS · {screen} "), style(Role::TitleBar)),
        Span::styled("█▓▒░", shade),
    ])
    .alignment(Alignment::Center)
}

/// The bar across the bottom: server, human and time online.
pub fn status_bar(server: &str, human: &str, online: Duration) -> Line<'static> {
    let secs = online.as_secs();
    let clock = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    let bar = style(Role::StatusBar);
    Line::from(vec![
        Span::styled(format!(" {server} "), bar.add_modifier(Modifier::BOLD)),
        Span::styled("│", bar),
        Span::styled(format!(" {human} "), bar),
        Span::styled("│", bar),
        Span::styled(format!(" online {clock} "), bar),
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
