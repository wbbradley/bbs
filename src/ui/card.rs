//! A message as a conversation card: the one way `bbs connect` and
//! `bbs view` draw mail. Mail between the viewer and the other side of a
//! conversation goes without a header; everyone else's is a side card naming
//! who sent it to whom; the viewer's own mail sits on their surface.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::proto::{Kind, Message};
use crate::ui::message::{
    compact_message_lines, conversation_roles, find_matches, message_prefix, side_header,
    side_message_lines,
};
use crate::ui::theme::{self, Role};
use crate::ui::thread::Row;

/// How a card draws its message.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    /// Mail between the viewer and the other side omits its header.
    pub headerless: bool,
    /// The recipient's kind, when the view knows it better than the message.
    pub recipient: Option<Kind>,
    /// The viewer's own mail, on their surface.
    pub outgoing: bool,
    /// Every header shows the local send time, group mail included.
    pub timed: bool,
    /// Unread direct mail says so at the card's foot.
    pub receipt: bool,
    /// A possible gap in the raw feed before this message.
    pub gap: bool,
}

impl Look {
    /// The board-wide feed: every message a side card with its time; the
    /// viewer's own on their surface; unread direct mail marked.
    pub fn feed(message: &Message, viewer: Option<&str>) -> Look {
        Look {
            outgoing: message.from_kind() == Kind::Human
                && viewer.is_some_and(|v| message.from_name.eq_ignore_ascii_case(v)),
            timed: true,
            receipt: true,
            ..Look::default()
        }
    }
}

/// The card's lines at `width`, or unwrapped without one.
pub fn lines<'a>(
    message: &'a Message,
    look: Look,
    gap: bool,
    query: Option<&str>,
    width: Option<u16>,
) -> Vec<Line<'a>> {
    let gap = gap || look.gap;
    let mut lines = if look.headerless {
        compact_message_lines(message, gap, query, width)
    } else {
        side_message_lines(message, gap, query, look.recipient, width, look.timed)
    };
    // Until its one reader reads it, the foot says so, inset from the right
    // as the body is from the left. Group mail has no one reader to say it.
    let unread = look.receipt && message.read_at.is_none() && message.to_group.is_none();
    if look.outgoing {
        if look.headerless {
            lines.insert(1 + usize::from(gap), Line::default());
        }
        lines.push(if unread { note() } else { Line::default() });
        for line in &mut lines {
            line.style = theme::style(Role::HumanSurface).patch(line.style);
        }
    } else if unread {
        lines.push(note());
    }
    lines
}

fn note() -> Line<'static> {
    Line::styled("unread  ", theme::style(Role::HumanNote)).right_aligned()
}

/// The surface across every row of the card.
pub fn style(message: &Message, look: Look) -> Style {
    if look.outgoing {
        theme::style(Role::HumanSurface)
    } else if look.headerless {
        Style::default()
    } else {
        theme::style(conversation_roles(message).0)
    }
}

/// The margin outside the wrapping area.
pub const INSET: u16 = 2;

/// A side card's header marker, in the gutter beside its header rows.
pub fn gutter(
    message: &Message,
    look: Look,
    gap: bool,
    query: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    if look.headerless {
        return Vec::new();
    }
    let measure = |lines: Vec<Line>| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .line_count(width)
    };
    let prefix_height = measure(message_prefix(gap || look.gap));
    let header_height = measure(side_header(
        message,
        query,
        look.recipient,
        Some(width),
        look.timed,
    ));
    let surface = theme::style(conversation_roles(message).1);
    let mut gutter = vec![Line::default(); prefix_height];
    for index in 0..header_height {
        gutter.push(
            Line::from(Span::styled(
                if index == 0 { "• " } else { "  " },
                surface.patch(theme::style(if message.to_group.is_some() {
                    Role::Group
                } else {
                    Role::Accent
                })),
            ))
            .style(surface),
        );
    }
    gutter
}

/// The blank row above a card keeps the screen's own surface.
pub fn separator_style(look: Look) -> Option<Style> {
    (!look.headerless || look.outgoing).then(|| theme::style(Role::Screen))
}

/// The sender, recipient, group, or body as sent (markdown included).
pub fn matches(message: &Message, query: &str) -> bool {
    [
        Some(message.qualified_sender()),
        message.qualified_recipient(),
        message.to_group.as_ref().map(|g| format!("@{g}")),
    ]
    .iter()
    .flatten()
    .map(String::as_str)
    .chain([message.body.as_str()])
    .any(|text| !find_matches(text, query).is_empty())
}

/// A message on a feed: `bbs view`'s rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub message: Message,
    pub look: Look,
}

impl Card {
    pub fn feed(message: Message, viewer: Option<&str>) -> Card {
        let look = Look::feed(&message, viewer);
        Card { message, look }
    }
}

impl Row for Card {
    type Key = i64;
    fn key(&self) -> i64 {
        self.message.id
    }
    fn lines(&self, gap: bool, query: Option<&str>) -> Vec<Line<'_>> {
        lines(&self.message, self.look, gap, query, None)
    }
    fn lines_at(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        lines(&self.message, self.look, gap, query, Some(width))
    }
    fn style(&self) -> Style {
        style(&self.message, self.look)
    }
    fn inset(&self) -> u16 {
        INSET
    }
    fn gutter(&self, gap: bool, query: Option<&str>, width: u16) -> Vec<Line<'_>> {
        gutter(&self.message, self.look, gap, query, width)
    }
    fn separator_style(&self) -> Option<Style> {
        separator_style(self.look)
    }
    fn matches(&self, query: &str) -> bool {
        matches(&self.message, query)
    }
}
