//! Embedded logo geometry, colored by the selected scheme.
use crate::ui::theme::{Role, style};
use ratatui::text::{Line, Text};
const LOGO: &str = include_str!("../../assets/connect/logo.txt");
pub fn logo() -> Text<'static> {
    Text::from(
        LOGO.lines()
            .map(|line| Line::styled(line, style(Role::Logo)))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_parses_and_fits_the_menu() {
        let logo = logo();
        assert!(logo.height() <= 8, "{}", logo.height());
        assert!(logo.width() <= 76, "{}", logo.width());
        let text: String = logo.lines[1].spans.iter().map(|s| &*s.content).collect();
        assert!(text.contains('█'), "{text:?}");
        assert_eq!(logo.lines[1].style, style(Role::Logo));
    }
}
