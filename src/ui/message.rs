//! A message as styled lines: its header and markdown body, with search
//! matches highlighted. Shared by `bbs view` and `bbs connect`.

use chrono::{DateTime, Local};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::proto::{Kind, Message};

/// Header spans that hold searchable text: the sender and the recipient or group.
const HEADER_NAMES: [usize; 2] = [2, 4];

/// A message as displayed: a blank separator, a marker when messages may be
/// missing before it, the header, and the indented markdown body, with
/// `query` matches highlighted in the names and body.
pub fn message_lines<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    width: u16,
) -> Vec<Line<'a>> {
    let mut header = header(row);
    let mut body = Vec::new();
    let source = literal_lines(&row.body);
    for line in tui_markdown::from_str(&source).lines {
        let mut line = Line {
            spans: line
                .spans
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), s.style))
                .collect(),
            style: line.style,
            alignment: line.alignment,
        };
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
        .chain(body.into_iter().flat_map(|line| wrap_body(line, width)))
        .collect()
}

/// Keep message line breaks while letting Markdown interpret its block syntax.
/// Use the same parser extensions as tui-markdown so code, tables and metadata
/// are not mistaken for ordinary paragraphs.
fn literal_lines(source: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_SUPERSCRIPT
        | Options::ENABLE_SUBSCRIPT
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_GFM
        | Options::ENABLE_TABLES;
    let mut edits = Vec::new();
    let mut containers = 0usize;
    let mut lists = Vec::new();
    let mut item_indents = Vec::new();
    let mut events = Parser::new_ext(source, options)
        .into_offset_iter()
        .peekable();
    while let Some((event, range)) = events.next() {
        match event {
            Event::Start(Tag::List(index)) => {
                containers += 1;
                lists.push(index);
            }
            Event::Start(Tag::BlockQuote(_) | Tag::DefinitionList) => containers += 1,
            Event::End(TagEnd::List(_)) => {
                containers -= 1;
                lists.pop();
            }
            Event::End(TagEnd::BlockQuote(_) | TagEnd::DefinitionList) => containers -= 1,
            Event::Start(Tag::Item) => {
                // Match tui-markdown's list marker width, including nesting and
                // multi-digit ordered markers, on the new continuation line.
                let marker_width = lists.len() * 4 - 3;
                let indent = match lists.last_mut() {
                    Some(Some(index)) => {
                        let width = marker_width.max(index.to_string().len()) + 2;
                        *index += 1;
                        width
                    }
                    _ => marker_width + 1,
                };
                item_indents.push(indent);
            }
            Event::End(TagEnd::Item) => {
                item_indents.pop();
            }
            Event::Start(Tag::Paragraph) if containers == 0 => {
                let start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
                let prefix = &source[start..range.start];
                if prefix.bytes().all(|b| matches!(b, b' ' | b'\t')) {
                    edits.push((range.start, "&#32;".repeat(indent_columns(prefix))));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if event == Event::SoftBreak {
                    edits.push((range.start, "  ".to_string()));
                }
                if let Some((_, next)) = events.peek()
                    && next.start >= range.end
                {
                    let prefix = &source[range.end..next.start];
                    let (spaces, quote) = prefix
                        .rsplit_once('>')
                        .map_or((prefix, false), |(_, s)| (s, true));
                    if spaces.bytes().all(|b| matches!(b, b' ' | b'\t')) {
                        let columns = indent_columns(spaces).saturating_sub(usize::from(quote));
                        let columns = columns.max(item_indents.last().copied().unwrap_or(0));
                        // Keep structural indentation in the source; entities
                        // restore only the visible spaces Markdown trims away.
                        edits.push((next.start, "&#32;".repeat(columns)));
                    }
                }
            }
            _ => {}
        }
    }
    edits.sort_by_key(|(at, _)| *at);
    let mut out = String::new();
    let mut at = 0;
    for (position, insertion) in edits {
        out.push_str(&source[at..position]);
        out.push_str(&insertion);
        at = position;
    }
    out.push_str(&source[at..]);
    out
}

/// Markdown tabs advance to four-column tab stops.
fn indent_columns(spaces: &str) -> usize {
    spaces.bytes().fold(0, |column, b| {
        column + if b == b'\t' { 4 - column % 4 } else { 1 }
    })
}

/// Wrap body lines with their visible indentation on every continuation, keeping
/// span styles (including search highlights) across the split.
fn wrap_body(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= usize::from(width) {
        return vec![line];
    }
    let text = line.to_string();
    let indent = text.bytes().take_while(|&b| b == b' ').count();
    // Leave room for a wide character, even in a very narrow pane.
    let indent = indent.min(usize::from(width.saturating_sub(2)));
    let options = textwrap::Options::new(usize::from(width) - indent)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let mut at = indent;
    textwrap::wrap(&text[indent..], options)
        .into_iter()
        .map(|part| {
            let start = at
                + text[at..]
                    .find(part.as_ref())
                    .expect("wrapping only splits the source");
            let end = start + part.len();
            at = end;
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            let mut offset = 0;
            for span in &line.spans {
                let lo = start.max(offset);
                let hi = end.min(offset + span.content.len());
                if lo < hi {
                    spans.push(Span::styled(
                        span.content[lo - offset..hi - offset].to_owned(),
                        span.style,
                    ));
                }
                offset += span.content.len();
            }
            Line {
                spans,
                style: line.style,
                alignment: line.alignment,
            }
        })
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

/// A stored time in local time, as message headers show it.
pub fn local_time(stored: &str) -> String {
    local(stored).map_or_else(
        || stored.to_string(),
        |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
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

    fn body(source: &str, width: u16, query: Option<&str>) -> Vec<Line<'static>> {
        let row = Message {
            body: source.into(),
            ..Message::default()
        };
        // The header borrows nothing from the row, and body spans own their text.
        message_lines(&row, false, query, width)
            .into_iter()
            .skip(2)
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect()
    }

    #[test]
    fn single_line_breaks_and_plain_indentation_survive_markdown() {
        for newline in ["\n", "\r\n"] {
            let source = format!("  **one**{newline} two{newline}\tthree{newline}{newline}last");
            let lines = body(&source, 80, None);
            let text: Vec<_> = lines.iter().map(ToString::to_string).collect();
            assert_eq!(text, ["    one", "   two", "      three", "  ", "  last"]);
            assert!(
                lines[0]
                    .spans
                    .iter()
                    .any(|s| s.content == "one" && s.style.add_modifier.contains(Modifier::BOLD))
            );
        }
        for source in ["one  \n  two", "one\\\n  two"] {
            assert_eq!(
                body(source, 80, None)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                ["  one", "    two"]
            );
        }
    }

    #[test]
    fn markdown_blocks_keep_their_formatting() {
        for source in [
            "# Heading\n\n- **one**\n  - two\n\n> quote",
            "```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
            "| one | two |\n| --- | --- |\n| a | b |",
            "---\nkey: value\n---\n\ntext",
        ] {
            let expected: Vec<_> = tui_markdown::from_str(source)
                .lines
                .into_iter()
                .map(|mut line| {
                    line.spans.insert(0, Span::raw("  "));
                    line
                })
                .collect();
            assert_eq!(body(source, 100, None), expected, "{source}");
        }
        assert_eq!(
            body("- one\n  continued\n  - nested\n    continued", 80, None)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "  - one",
                "    continued",
                "      - nested",
                "        continued"
            ]
        );
    }

    #[test]
    fn wrapped_body_keeps_indentation_styles_and_search_highlights() {
        let lines = body("  **alpha beta gamma delta**", 15, Some("gamma"));
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["    alpha beta", "    gamma delta"]
        );
        for line in &lines {
            for span in &line.spans {
                if !span.content.trim().is_empty() {
                    assert!(span.style.add_modifier.contains(Modifier::BOLD));
                }
                if span.content == "gamma" {
                    assert!(span.style.add_modifier.contains(Modifier::REVERSED));
                }
            }
        }
        let lines = body("界界界界界界", 8, None);
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["  界界界", "  界界界"]
        );
        for width in [0, 1, 2, 3, 5] {
            let lines = body("  abc界", width, None);
            assert!(!lines.is_empty());
        }
    }

    #[test]
    fn highlight_keeps_combining_marks_with_their_letter() {
        let spans = highlight(Span::raw("e\u{301}cole"), "e");
        assert_eq!(spans[0].content, "e\u{301}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "e\u{301}cole");
    }
}
