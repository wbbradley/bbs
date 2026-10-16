//! ANSI art from `assets/connect/`, embedded in the binary. The files are
//! UTF-8 text with 16-colour SGR codes, as ANSI editors such as Moebius and
//! PabloDraw export them.

use ansi_to_tui::IntoText as _;
use ratatui::text::Text;

const LOGO: &[u8] = include_bytes!("../../assets/connect/logo.ans");

/// The board's logo.
pub fn logo() -> Text<'static> {
    LOGO.into_text()
        .expect("assets/connect/logo.ans is valid ANSI")
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
        assert!(logo.lines[1].spans.iter().any(|s| s.style.fg.is_some()));
    }
}
