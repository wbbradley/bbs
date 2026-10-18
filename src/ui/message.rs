//! A message as styled lines: its header and markdown body, with search
//! matches highlighted. Shared by `bbs view` and `bbs connect`.

use crate::ui::theme::{Role, style};
use chrono::{DateTime, Local};
use ratatui::style::{Modifier, Style, Styled};
use ratatui::text::{Line, Span};

use crate::proto::{Kind, Message};

/// Header spans that hold searchable text: the sender and the recipient or group.
const HEADER_NAMES: [usize; 2] = [2, 4];

/// A message as displayed: a blank separator, a marker when messages may be
/// missing before it, the header, and the indented markdown body, with
/// `query` matches highlighted in the names and body.
pub fn message_lines<'a>(row: &'a Message, gap: bool, query: Option<&str>) -> Vec<Line<'a>> {
    with_header(row, gap, query, Some(header(row)))
}

/// The same body and search rendering, without a header.
pub fn compact_message_lines<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
) -> Vec<Line<'a>> {
    with_header(row, gap, query, None)
}

fn with_header<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    mut header: Option<Line<'static>>,
) -> Vec<Line<'a>> {
    let mut body = Vec::new();
    for mut line in crate::ui::markdown::render(&row.body).lines {
        line.spans.insert(0, Span::raw("  "));
        body.push(line);
    }
    if let Some(query) = query {
        if let Some(header) = &mut header {
            header.spans = std::mem::take(&mut header.spans)
                .into_iter()
                .enumerate()
                .flat_map(|(i, span)| match HEADER_NAMES.contains(&i) {
                    true => highlight(span, query),
                    false => vec![span],
                })
                .collect();
        }
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
                .set_style(style(Role::Warning)),
        )
    });
    [Line::default()]
        .into_iter()
        .chain(marker)
        .chain(header)
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
            style(Role::Muted),
        ),
        Span::raw("  "),
        name_span(&row.qualified_sender(), row.from_kind()),
        Span::styled(" → ", style(Role::Muted)),
    ];
    match (&row.to_group, row.qualified_recipient()) {
        (Some(group), _) => spans.push(Span::styled(
            format!("@{group}"),
            style(Role::Warning).add_modifier(Modifier::BOLD),
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
                    Span::styled(format!("read {}", read.format(fmt)), style(Role::Success))
                }
                None => Span::styled("unread", style(Role::Error)),
            });
        }
        (None, None) => spans.push(Span::raw("?")),
    }
    Line::from(spans)
}

fn name_span(name: &str, kind: Kind) -> Span<'static> {
    let author = match kind {
        Kind::Human => Role::Human,
        Kind::Agent | Kind::Unspecified => Role::OwnAgent,
    };
    Span::styled(name.to_string(), style(author).add_modifier(Modifier::BOLD))
}

fn local(stored: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

/// How long ago `stored` was, as at most two nonzero day/hour/minute units
/// plus "ago": "14m ago", "1h53m ago", "3d4h ago" (never "3d4h17m ago").
/// Under a minute, or in the future from clock skew, shows "0m ago". A
/// missing or unparseable time stays blank.
pub fn age(stored: &str, now: DateTime<Local>) -> String {
    let Some(t) = local(stored) else {
        return String::new();
    };
    let minutes = (now.signed_duration_since(t).num_seconds().max(0) / 60) as u64;
    let units = [(minutes / 1440, "d"), (minutes % 1440 / 60, "h"), (minutes % 60, "m")];
    let shown: Vec<String> = units
        .iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    match shown.is_empty() {
        true => "0m ago".to_string(),
        false => format!("{} ago", shown.join("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_omits_headers_and_keeps_body_styles_search_and_gap_markers() {
        let row = Message {
            created_at: "2026-10-16T18:00:00Z".into(),
            from_name: "Mateo".into(),
            to_name: Some("ada".into()),
            body: "**answer**".into(),
            ..Default::default()
        };
        let full = message_lines(&row, false, Some("answer"));
        let compact = compact_message_lines(&row, false, Some("answer"));
        assert_eq!(&compact[1..], &full[2..]);
        assert_eq!(compact.len() + 1, full.len());
        assert!(
            compact
                .iter()
                .all(|line| !line.to_string().contains("2026-"))
        );
        let with_gap = compact_message_lines(&row, true, Some("answer"));
        assert!(with_gap[1].to_string().contains("messages may be missing"));
        assert_eq!(&with_gap[2..], &compact[1..]);
        let answer = compact[1]
            .spans
            .iter()
            .find(|s| s.content == "answer")
            .unwrap();
        assert!(
            answer
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::REVERSED)
        );
    }

    #[test]
    fn age_shows_at_most_two_nonzero_units() {
        let now = DateTime::parse_from_rfc3339("2026-10-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        let cases = [
            ("", ""),
            ("not a time", ""),
            // Future clock skew clamps to zero.
            ("2026-10-18T12:00:01Z", "0m ago"),
            ("2026-10-18T12:00:00Z", "0m ago"),
            ("2026-10-18T11:59:01Z", "0m ago"),
            ("2026-10-18T11:59:00Z", "1m ago"),
            ("2026-10-18T11:46:00Z", "14m ago"),
            ("2026-10-18T11:00:00Z", "1h ago"),
            ("2026-10-18T10:07:00Z", "1h53m ago"),
            ("2026-10-17T12:00:00Z", "1d ago"),
            ("2026-10-17T11:00:00Z", "1d1h ago"),
            // The third unit is dropped, and zero units are skipped.
            ("2026-10-15T07:43:00Z", "3d4h ago"),
            ("2026-10-15T11:43:00Z", "3d17m ago"),
            ("2026-10-11T12:01:00Z", "6d23h ago"),
        ];
        for (stored, want) in cases {
            assert_eq!(age(stored, now), want, "{stored}");
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
