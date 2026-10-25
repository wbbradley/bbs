//! The app's screens.

pub mod agents;
pub mod blockers;
pub mod config;
pub mod groups;
pub mod help;
pub mod menu;
pub mod thread;

use ratatui::widgets::TableState;

use super::keys::Action;

/// Move a list's selection for a navigation action; `page` is the number of
/// rows in view. Returns whether the action was a navigation action.
pub fn navigate(state: &mut TableState, len: usize, page: usize, action: Action) -> bool {
    let last = len.saturating_sub(1);
    let at = state.selected().unwrap_or(0);
    let page = page.max(1);
    let to = match action {
        Action::Up => at.saturating_sub(1),
        Action::Down => at + 1,
        Action::Top => 0,
        Action::Bottom => last,
        Action::HalfPageUp => at.saturating_sub(page / 2),
        Action::HalfPageDown => at + page / 2,
        Action::PageUp => at.saturating_sub(page),
        Action::PageDown => at + page,
        _ => return false,
    };
    state.select((len > 0).then_some(to.min(last)));
    true
}

/// `text` cut to `width` columns, ending with … when cut.
pub fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if unicode_width::UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// A structured list keeps each whole item's styles when the +N suffix fits.
pub fn fit_styled_list(
    items: Vec<ratatui::text::Line<'static>>,
    width: usize,
) -> ratatui::text::Line<'static> {
    use crate::ui::theme::{Role, style};
    use ratatui::text::{Line, Span};
    let joined_width =
        |n: usize| items[..n].iter().map(Line::width).sum::<usize>() + n.saturating_sub(1) * 2;
    let all = joined_width(items.len()) <= width;
    let shown = if all {
        items.len()
    } else {
        (1..items.len())
            .rev()
            .find(|n| joined_width(*n) + format!(" +{}", items.len() - n).len() <= width)
            .unwrap_or(0)
    };
    if shown == 0 && !items.is_empty() {
        return Line::styled(fit(&format!("+{}", items.len()), width), style(Role::Muted));
    }
    let mut spans = Vec::new();
    for (i, item) in items.iter().take(shown).enumerate() {
        if i > 0 {
            spans.push(Span::raw(", "));
        }
        spans.extend(item.spans.iter().cloned().map(|mut span| {
            span.style = item.style.patch(span.style);
            span
        }));
    }
    if !all {
        spans.push(Span::styled(
            format!(" +{}", items.len() - shown),
            style(Role::Muted),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_stays_in_the_list() {
        let mut state = TableState::default();
        assert!(navigate(&mut state, 10, 4, Action::Down));
        assert_eq!(state.selected(), Some(1));
        navigate(&mut state, 10, 4, Action::PageDown);
        assert_eq!(state.selected(), Some(5));
        navigate(&mut state, 10, 4, Action::HalfPageDown);
        assert_eq!(state.selected(), Some(7));
        navigate(&mut state, 10, 4, Action::PageDown);
        assert_eq!(state.selected(), Some(9));
        navigate(&mut state, 10, 4, Action::Top);
        assert_eq!(state.selected(), Some(0));
        navigate(&mut state, 10, 4, Action::Up);
        assert_eq!(state.selected(), Some(0));
        navigate(&mut state, 0, 4, Action::Down);
        assert_eq!(state.selected(), None);
        assert!(!navigate(&mut state, 10, 4, Action::Open));
    }

    #[test]
    fn cells_end_with_an_ellipsis_and_lists_keep_whole_names() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("a long status line", 8), "a long …");
        assert_eq!(fit("日本語テキスト", 7), "日本語…");
        let groups: Vec<String> = ["acme", "beta", "gamma"].map(String::from).into();
        let fit_list = |groups: &[String], width| {
            fit_styled_list(
                groups
                    .iter()
                    .cloned()
                    .map(ratatui::text::Line::raw)
                    .collect(),
                width,
            )
            .to_string()
        };
        assert_eq!(fit_list(&groups, 20), "acme, beta, gamma");
        assert_eq!(fit_list(&groups, 16), "acme, beta +1");
        assert_eq!(fit_list(&groups, 8), "acme +2");
        assert_eq!(fit_list(&groups, 3), "+3");
    }
}
