//! A message as styled lines: its header and markdown body, with search
//! matches highlighted. Shared by `bbs view` and `bbs connect`.

use crate::ui::theme::{Role, style};
use chrono::{DateTime, Local};
use ratatui::style::{Modifier, Styled};
use ratatui::text::{Line, Span};

use crate::proto::{Kind, Message};

/// A message as displayed: a blank separator, a marker when messages may be
/// missing before it, the header, and the indented markdown body, with
/// `query` matches highlighted in the names and body.
pub fn message_lines<'a>(row: &'a Message, gap: bool, query: Option<&str>) -> Vec<Line<'a>> {
    message_lines_with_recipient(row, gap, query, None)
}

/// A conversation can identify a bare recipient from its typed target/human.
/// Board-wide views leave absent recipient metadata neutral.
pub fn message_lines_with_recipient<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    recipient: Option<Kind>,
) -> Vec<Line<'a>> {
    message_lines_with_recipient_at(row, gap, query, recipient, None)
}

pub fn message_lines_with_recipient_at<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    recipient: Option<Kind>,
    width: Option<u16>,
) -> Vec<Line<'a>> {
    with_header(
        row,
        gap,
        query,
        vec![header(row, query, recipient)],
        true,
        width,
    )
}

/// A side conversation identifies its participants without delivery metadata
/// or owner suffixes. The stored message and ordinary feed header stay intact.
pub fn side_message_lines<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    recipient: Option<Kind>,
    width: Option<u16>,
) -> Vec<Line<'a>> {
    with_header(
        row,
        gap,
        query,
        side_header(row, query, recipient, width),
        false,
        width,
    )
}

/// Header-only rendering lets the conversation lay out its gutter without
/// parsing the message body a second time.
/// The header's rows. Direct mail between others shows when it was sent, in
/// local time, at the right of a header `width` wide: beside the names when
/// they leave room, else on a row of its own above them. Group mail, or a
/// header too narrow for the time, shows none.
pub fn side_header(
    row: &Message,
    query: Option<&str>,
    recipient: Option<Kind>,
    width: Option<u16>,
) -> Vec<Line<'static>> {
    let sender = if row.from_kind() == Kind::Unspecified && row.from_owner.is_some() {
        Kind::Agent
    } else {
        row.from_kind()
    };
    let mut spans = highlight_names(name_spans(&row.from_name, None, sender), query);
    spans.push(Span::styled(" → ", style(Role::Muted)));
    match (&row.to_group, row.to_name.as_deref()) {
        (Some(group), _) => spans.extend(highlight_names(
            vec![Span::styled(format!("@{group}"), style(Role::Group).bold())],
            query,
        )),
        (None, Some(to)) => spans.extend(highlight_names(
            name_spans(
                to,
                None,
                recipient.unwrap_or(if row.to_owner.is_some() {
                    Kind::Agent
                } else {
                    Kind::Unspecified
                }),
            ),
            query,
        )),
        (None, None) => spans.push(Span::raw("?")),
    }
    let surface = style(conversation_roles(row).1);
    let mut names = Line::from(spans).style(surface);
    let (Some(width), None, Some(sent)) = (width, &row.to_group, local(&row.created_at)) else {
        return vec![names];
    };
    let sent = sent.format("%Y-%m-%d %-I:%M:%S%P").to_string();
    let width = usize::from(width);
    // ASCII digits and punctuation: one column per byte.
    let room = width.saturating_sub(names.width() + sent.len());
    if room >= 2 {
        names.spans.push(Span::raw(" ".repeat(room)));
        names.spans.push(Span::styled(sent, style(Role::Timestamp)));
        return vec![names];
    }
    if sent.len() > width {
        return vec![names];
    }
    let time = Line::from(vec![
        Span::raw(" ".repeat(width - sent.len())),
        Span::styled(sent, style(Role::Timestamp)),
    ])
    .style(surface);
    vec![time, names]
}

/// Semantic surfaces for a conversation card, independent of its author.
pub fn conversation_roles(row: &Message) -> (Role, Role) {
    if row.to_group.is_some() {
        (Role::GroupConversation, Role::GroupConversationHeader)
    } else {
        (Role::OtherConversation, Role::OtherConversationHeader)
    }
}

/// The body and search rendering without a header or literal inset; the
/// conversation supplies its margin so wrapped lines keep the same inset.
pub fn compact_message_lines<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    width: Option<u16>,
) -> Vec<Line<'a>> {
    with_header(row, gap, query, Vec::new(), false, width)
}

fn with_header<'a>(
    row: &'a Message,
    gap: bool,
    query: Option<&str>,
    header: Vec<Line<'static>>,
    indent: bool,
    width: Option<u16>,
) -> Vec<Line<'a>> {
    let mut body = Vec::new();
    for mut line in crate::ui::markdown::render_at(
        &row.body,
        width.map(|w| w.saturating_sub(if indent { 2 } else { 0 })),
    )
    .lines
    {
        if indent {
            line.spans.insert(0, Span::raw("  "));
        }
        body.push(line);
    }
    if let Some(query) = query {
        for line in &mut body {
            line.spans = std::mem::take(&mut line.spans)
                .into_iter()
                .flat_map(|span| highlight(span, query))
                .collect();
        }
    }
    message_prefix(gap)
        .into_iter()
        .chain(header)
        .chain(body)
        .collect()
}

/// The separator and optional missing-message warning before a header/body.
pub fn message_prefix(gap: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    if gap {
        lines.push(Line::from(
            "  ⋯ messages may be missing here: more than --limit arrived at once; use --since/--until or a larger --limit"
                .set_style(style(Role::Warning)),
        ));
    }
    lines
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

fn header(row: &Message, query: Option<&str>, recipient: Option<Kind>) -> Line<'static> {
    let sent = local(&row.created_at);
    let mut spans = vec![
        Span::styled(
            sent.map_or_else(
                || row.created_at.clone(),
                |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            style(Role::Timestamp),
        ),
        Span::raw("  "),
    ];
    spans.extend(highlight_names(
        name_spans(&row.from_name, row.from_owner.as_deref(), row.from_kind()),
        query,
    ));
    spans.push(Span::styled(" → ", style(Role::Muted)));
    match (&row.to_group, row.to_name.as_deref()) {
        (Some(group), _) => spans.extend(highlight_names(
            vec![Span::styled(
                format!("@{group}"),
                style(Role::Group).add_modifier(Modifier::BOLD),
            )],
            query,
        )),
        (None, Some(to)) => {
            let kind = recipient.unwrap_or(if row.to_owner.is_some() {
                Kind::Agent
            } else {
                Kind::Unspecified
            });
            spans.extend(highlight_names(
                name_spans(to, row.to_owner.as_deref(), kind),
                query,
            ));
            spans.push(Span::raw("  "));
            match row.read_at.as_deref().and_then(local) {
                Some(read) => {
                    let same_day = sent.is_some_and(|s| s.date_naive() == read.date_naive());
                    let fmt = if same_day {
                        "%H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M:%S"
                    };
                    spans.push(Span::styled("read ", style(Role::Success)));
                    spans.push(Span::styled(
                        read.format(fmt).to_string(),
                        style(Role::Timestamp),
                    ));
                }
                None => spans.push(Span::styled("unread", style(Role::Error))),
            }
        }
        (None, None) => spans.push(Span::raw("?")),
    }
    Line::from(spans)
}

/// Only structured identity metadata decides a category; unknown names stay text.
pub fn name_spans(name: &str, owner: Option<&str>, kind: Kind) -> Vec<Span<'static>> {
    let role = match kind {
        Kind::Human => Role::Human,
        Kind::Agent => Role::Agent,
        Kind::Unspecified if owner.is_some() => Role::Agent,
        Kind::Unspecified => Role::Text,
    };
    let mut spans = vec![Span::styled(
        name.to_string(),
        style(role).add_modifier(Modifier::BOLD),
    )];
    if kind != Kind::Human
        && let Some(owner) = owner
    {
        spans.push(Span::raw("@"));
        spans.push(Span::styled(
            owner.to_string(),
            style(Role::Human).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// Match the entire qualified field, then retain each category's style when
/// splitting the highlighted ranges, including a match crossing `agent@owner`.
fn highlight_names(spans: Vec<Span<'static>>, query: Option<&str>) -> Vec<Span<'static>> {
    let Some(query) = query else { return spans };
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let ranges = find_matches(&text, query);
    let mut offset = 0;
    let mut out = Vec::new();
    for span in spans {
        let end = offset + span.content.len();
        let mut at = 0;
        for range in &ranges {
            let start = range.start.max(offset);
            let stop = range.end.min(end);
            if start >= stop {
                continue;
            }
            let start = start - offset;
            let stop = stop - offset;
            if start > at {
                out.push(Span::styled(
                    span.content[at..start].to_string(),
                    span.style,
                ));
            }
            out.push(Span::styled(
                span.content[start..stop].to_string(),
                span.style.add_modifier(Modifier::REVERSED),
            ));
            at = stop;
        }
        if at < span.content.len() {
            out.push(Span::styled(span.content[at..].to_string(), span.style));
        }
        offset = end;
    }
    out
}

fn local(stored: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

/// How long ago `stored` was, as at most two nonzero day/hour/minute units
/// plus "ago": "14m ago", "1h53m ago", "3d4h ago" (never "3d4h17m ago").
/// Under a minute, or in the future from clock skew, shows "now". A
/// missing or unparseable time stays blank.
pub fn age(stored: &str, now: DateTime<Local>) -> String {
    let Some(t) = local(stored) else {
        return String::new();
    };
    let minutes = (now.signed_duration_since(t).num_seconds().max(0) / 60) as u64;
    let units = [
        (minutes / 1440, "d"),
        (minutes % 1440 / 60, "h"),
        (minutes % 60, "m"),
    ];
    let shown: Vec<String> = units
        .iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    match shown.is_empty() {
        true => "now".to_string(),
        false => format!("{} ago", shown.join("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process time zone's rendering of a stored time, as the header shows it.
    fn local_text(stored: &str) -> String {
        local(stored)
            .unwrap()
            .format("%Y-%m-%d %-I:%M:%S%P")
            .to_string()
    }

    #[test]
    fn direct_side_headers_show_the_local_send_time_at_the_right() {
        let direct = |created_at: &str| Message {
            created_at: created_at.into(),
            from_kind: Kind::Agent.into(),
            from_name: "Zed".into(),
            to_name: Some("Mateo".into()),
            ..Default::default()
        };
        let text = |lines: &[Line]| lines.iter().map(Line::to_string).collect::<Vec<_>>();
        // Early UTC on the 17th: a western zone shows the 16th, an eastern
        // the 17th; either way the header shows the process's local time.
        let row = direct("2026-10-17T03:30:00Z");
        let sent = local_text(&row.created_at);
        if Local::now().offset().local_minus_utc() != 0 {
            assert_ne!(sent, "2026-10-17 3:30:00am", "local, not UTC");
        }
        // Midnight and noon both use 12; afternoon uses a lowercase suffix.
        for (hour, expected) in [
            (0, "2026-10-17 12:07:08am"),
            (12, "2026-10-17 12:07:08pm"),
            (15, "2026-10-17 3:07:08pm"),
        ] {
            use chrono::TimeZone;
            let local = Local
                .with_ymd_and_hms(2026, 10, 17, hour, 7, 8)
                .single()
                .unwrap();
            let header = side_header(&direct(&local.to_rfc3339()), None, None, Some(60));
            assert_eq!(header[0].width(), 60);
            assert!(header[0].to_string().ends_with(expected), "{:?}", header);
        }
        let header = side_header(&row, None, Some(Kind::Agent), Some(60));
        assert_eq!(header.len(), 1, "beside the names when they leave room");
        let line = &header[0];
        assert_eq!(line.width(), 60);
        assert!(line.to_string().starts_with("Zed → Mateo "), "{line}");
        assert!(line.to_string().ends_with(&sent), "{line}");
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            style(Role::Timestamp).fg
        );
        assert_eq!(line.style, style(Role::OtherConversationHeader));
        assert_eq!(line.spans[0].style.fg, style(Role::Agent).fg);
        // Exactly two spaces of gap fit; with less, the time takes a row of
        // its own above the whole names, both on the header's surface.
        let names = "Zed → Mateo".chars().count();
        let fits = (names + 2 + sent.len()) as u16;
        assert_eq!(side_header(&row, None, None, Some(fits)).len(), 1);
        let tight = side_header(&row, None, None, Some(fits - 1));
        let width = usize::from(fits - 1);
        assert_eq!(
            text(&tight),
            [
                format!("{}{sent}", " ".repeat(width - sent.len())),
                "Zed → Mateo".into()
            ]
        );
        assert!(
            tight
                .iter()
                .all(|l| l.style == style(Role::OtherConversationHeader))
        );
        assert_eq!(tight[0].spans[1].style.fg, style(Role::Timestamp).fg);
        // Narrower than the time itself, or no width, no time, a bad time,
        // or group mail: the names alone, as before.
        let tiny = side_header(&row, None, None, Some(sent.len() as u16 - 1));
        assert_eq!(text(&tiny), ["Zed → Mateo"]);
        for (row, width) in [
            (direct("2026-10-17T03:30:00Z"), None),
            (direct(""), Some(80)),
            (direct("yesterday"), Some(80)),
            (
                Message {
                    to_group: Some("crew".into()),
                    to_name: None,
                    ..direct("2026-10-17T03:30:00Z")
                },
                Some(80),
            ),
        ] {
            let header = side_header(&row, None, None, width);
            assert_eq!(header.len(), 1);
            let line = header[0].to_string();
            assert!(!line.contains("2026") && !line.ends_with(' '), "{line:?}");
        }
    }

    #[test]
    fn side_headers_hide_owner_labels_without_losing_identity_styles() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut row = Message {
                    from_name: "Tim".into(),
                    from_owner: Some("ada".into()),
                    to_name: Some("Theo".into()),
                    to_owner: Some("ada".into()),
                    body: "2026-10-19 Tim@ada remains in the body".into(),
                    ..Default::default()
                };
                let lines = side_message_lines(&row, false, Some("tim"), None, None);
                assert_eq!(lines[1].to_string(), "Tim → Theo");
                assert_eq!(lines[1].spans[0].style.fg, style(Role::Agent).fg);
                assert!(
                    lines[1].spans[0]
                        .style
                        .add_modifier
                        .contains(Modifier::REVERSED)
                );
                assert!(
                    lines
                        .iter()
                        .any(|line| line.to_string().contains("2026-10-19 Tim@ada"))
                );
                row.to_group = Some("crew".into());
                assert_eq!(
                    side_message_lines(&row, false, None, None, None)[1].to_string(),
                    "Tim → @crew"
                );
            });
        }
    }

    #[test]
    fn typed_names_and_qualified_search_keep_their_categories() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let mut row = Message {
                    from_name: "Mateo".into(),
                    from_owner: Some("ada".into()),
                    from_kind: Kind::Agent.into(),
                    to_name: Some("Zed".into()),
                    to_owner: Some("ben".into()),
                    created_at: "2026-10-18T12:00:00Z".into(),
                    read_at: Some("2026-10-18T12:01:00Z".into()),
                    ..Default::default()
                };
                let line = header(&row, Some("eo@ad"), None);
                assert!(line.to_string().contains("Mateo@ada → Zed@ben"));
                let reversed: String = line
                    .spans
                    .iter()
                    .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
                    .map(|s| s.content.as_ref())
                    .collect();
                assert_eq!(reversed, "eo@ad");
                for (text, role) in [
                    ("Mat", Role::Agent),
                    ("ad", Role::Human),
                    ("Zed", Role::Agent),
                    ("ben", Role::Human),
                    ("read ", Role::Success),
                ] {
                    assert_eq!(
                        line.spans
                            .iter()
                            .find(|s| s.content == text)
                            .unwrap()
                            .style
                            .fg,
                        style(role).fg
                    );
                }
                assert_eq!(line.spans[0].style, style(Role::Timestamp));
                assert_eq!(line.spans.last().unwrap().style, style(Role::Timestamp));
                row.to_owner = None;
                let unknown = header(&row, None, None);
                assert_eq!(
                    unknown
                        .spans
                        .iter()
                        .find(|s| s.content == "Zed")
                        .unwrap()
                        .style
                        .fg,
                    style(Role::Text).fg
                );
                let human = header(&row, None, Some(Kind::Human));
                assert_eq!(
                    human
                        .spans
                        .iter()
                        .find(|s| s.content == "Zed")
                        .unwrap()
                        .style
                        .fg,
                    style(Role::Human).fg
                );
                row.to_group = Some("crew".into());
                let group = header(&row, None, None);
                assert_eq!(
                    group
                        .spans
                        .iter()
                        .find(|s| s.content == "@crew")
                        .unwrap()
                        .style
                        .fg,
                    style(Role::Group).fg
                );
                assert!(
                    !compact_message_lines(&row, false, None, None)
                        .iter()
                        .any(|l| l.to_string().contains("Mateo"))
                );
            });
        }
    }

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
        let compact = compact_message_lines(&row, false, Some("answer"), None);
        let mut body = full[2..].to_vec();
        for line in &mut body {
            line.spans.remove(0);
        }
        assert_eq!(&compact[1..], &body);
        assert_eq!(compact.len() + 1, full.len());
        assert!(
            compact
                .iter()
                .all(|line| !line.to_string().contains("2026-"))
        );
        let with_gap = compact_message_lines(&row, true, Some("answer"), None);
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
            ("2026-10-18T12:00:01Z", "now"),
            ("2026-10-18T12:00:00Z", "now"),
            ("2026-10-18T11:59:01Z", "now"),
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
