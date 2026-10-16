//! A message as styled lines: its header and markdown body, with search
//! matches highlighted. Shared by `bbs view` and `bbs connect`.

use chrono::{DateTime, Local};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::proto::{Kind, Message};

/// Header spans that hold searchable text: the sender and the recipient or group.
const HEADER_NAMES: [usize; 2] = [2, 4];

/// A message as displayed: a blank separator, a marker when messages may be
/// missing before it, the header, and the indented markdown body, with
/// `query` matches highlighted in the names and body.
pub fn message_lines<'a>(row: &'a Message, gap: bool, query: Option<&str>) -> Vec<Line<'a>> {
    let mut header = header(row);
    let mut body = Vec::new();
    for mut line in tui_markdown::from_str(&row.body).lines {
        line.spans.insert(0, Span::raw("  "));
        body.push(line);
    }
    if let Some(query) = query {
        header.spans = header
            .spans
            .into_iter()
            .enumerate()
            .flat_map(|(i, span)| match HEADER_NAMES.contains(&i) {
                true => highlight(span, query),
                false => vec![span],
            })
            .collect();
        for line in &mut body {
            line.spans = std::mem::take(&mut line.spans)
                .into_iter()
                .flat_map(|span| highlight(span, query))
                .collect();
        }
    }
    let marker = gap.then(|| {
        Line::from(
            "  ⋯ messages may be missing here: more than --limit arrived at once; use --since/--until or a larger --limit"
                .yellow(),
        )
    });
    [Line::default()]
        .into_iter()
        .chain(marker)
        .chain([header])
        .chain(body)
        .collect()
}

/// Case folding for search: a character's lowercase form when that is one
/// character, so folded text keeps a character-for-character correspondence.
pub fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// Byte ranges of non-overlapping case-insensitive occurrences of the folded
/// `query`, compared character by character so ranges always fall on
/// character boundaries of `text`.
pub fn find_matches(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let query: Vec<char> = query.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while !query.is_empty() && i + query.len() <= chars.len() {
        let hit = chars[i..i + query.len()]
            .iter()
            .zip(&query)
            .all(|(&(_, c), &q)| fold(c) == q);
        if hit {
            // Keep combining marks with the character they decorate.
            let mut next = i + query.len();
            while chars.get(next).is_some_and(|&(_, c)| is_combining(c)) {
                next += 1;
            }
            let end = chars.get(next).map_or(text.len(), |&(b, _)| b);
            found.push(chars[i].0..end);
            i = next;
        } else {
            i += 1;
        }
    }
    found
}

/// Common combining diacritical mark blocks.
fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Split a span so every `query` match is in reverse video.
pub fn highlight<'a>(span: Span<'a>, query: &str) -> Vec<Span<'a>> {
    let found = find_matches(&span.content, query);
    if found.is_empty() {
        return vec![span];
    }
    let content = span.content.to_string();
    let mut spans = Vec::new();
    let mut at = 0;
    for range in found {
        if range.start > at {
            spans.push(Span::styled(
                content[at..range.start].to_string(),
                span.style,
            ));
        }
        spans.push(Span::styled(
            content[range.clone()].to_string(),
            span.style.add_modifier(Modifier::REVERSED),
        ));
        at = range.end;
    }
    if at < content.len() {
        spans.push(Span::styled(content[at..].to_string(), span.style));
    }
    spans
}

fn header(row: &Message) -> Line<'static> {
    let sent = local(&row.created_at);
    let mut spans = vec![
        Span::styled(
            sent.map_or_else(
                || row.created_at.clone(),
                |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        name_span(&row.qualified_sender(), row.from_kind()),
        Span::styled(" → ", Style::new().fg(Color::DarkGray)),
    ];
    match (&row.to_group, row.qualified_recipient()) {
        (Some(group), _) => spans.push(Span::styled(
            format!("@{group}"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, Some(to)) => {
            spans.push(Span::styled(to, Style::new().add_modifier(Modifier::BOLD)));
            spans.push(Span::raw("  "));
            spans.push(match row.read_at.as_deref().and_then(local) {
                Some(read) => {
                    let same_day = sent.is_some_and(|s| s.date_naive() == read.date_naive());
                    let fmt = if same_day {
                        "%H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M:%S"
                    };
                    Span::styled(
                        format!("read {}", read.format(fmt)),
                        Style::new().fg(Color::Green),
                    )
                }
                None => Span::styled("unread", Style::new().fg(Color::Red)),
            });
        }
        (None, None) => spans.push(Span::raw("?")),
    }
    Line::from(spans)
}

fn name_span(name: &str, kind: Kind) -> Span<'static> {
    let color = match kind {
        Kind::Human => Color::Magenta,
        Kind::Agent | Kind::Unspecified => Color::Cyan,
    };
    Span::styled(
        name.to_string(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn local(stored: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_keeps_combining_marks_with_their_letter() {
        let spans = highlight(Span::raw("e\u{301}cole"), "e");
        assert_eq!(spans[0].content, "e\u{301}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "e\u{301}cole");
    }
}
