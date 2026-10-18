//! Startup-selected semantic styles shared by every terminal renderer.

use anyhow::{Result, bail};
use ratatui::style::{Color, Style};
use std::sync::OnceLock;

/// Components choose a purpose, never a palette color. Foreground-only roles
/// inherit their enclosing surface, including human input and outgoing rows.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Screen,
    Text,
    Muted,
    Technical,
    Heading,
    Accent,
    HumanSurface,
    OwnAgent,
    OtherAgent,
    Human,
    Hotkey,
    /// Agent or group addressed by the composer.
    Target,
    /// Keyboard guidance, distinct from message text and target labels.
    KeyHint,
    Border,
    Selection,
    Cursor,
    Success,
    Warning,
    Error,
    TitleBar,
    StatusBar,
    Logo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scheme {
    #[default]
    Gruvbox,
    Terminal,
    NoColor,
}

static SCHEME: OnceLock<Scheme> = OnceLock::new();

/// Explicit selectors override NO_COLOR; empty environment values are absent.
pub fn resolve(cli: Option<&str>, env: Option<&str>, no_color: bool) -> Result<Scheme> {
    match cli.or(env.filter(|s| !s.is_empty())) {
        Some("gruvbox-dark-medium") => Ok(Scheme::Gruvbox),
        Some("terminal") => Ok(Scheme::Terminal),
        Some("no-color") => Ok(Scheme::NoColor),
        Some(_) => bail!("unknown theme; choose gruvbox-dark-medium, terminal, or no-color"),
        None if no_color => Ok(Scheme::NoColor),
        None => Ok(Scheme::Gruvbox),
    }
}

pub fn init(cli: Option<&str>) -> Result<()> {
    let env = std::env::var("BBS_THEME").ok();
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let scheme = resolve(cli, env.as_deref(), no_color)?;
    SCHEME
        .set(scheme)
        .expect("theme initialized once before rendering");
    Ok(())
}

pub fn style(role: Role) -> Style {
    #[cfg(test)]
    if let Some(scheme) = testing::OVERRIDE.get() {
        return scheme.style(role);
    }
    SCHEME.get().copied().unwrap_or_default().style(role)
}

impl Scheme {
    pub fn style(self, role: Role) -> Style {
        use Role::*;
        if self == Self::NoColor {
            return match role {
                Screen => Style::new().fg(Color::Reset).bg(Color::Reset),
                Muted => Style::new().dim(),
                KeyHint => Style::new().italic(),
                Heading | OwnAgent | OtherAgent | Human | Hotkey | Target | Success | Warning
                | Error => Style::new().bold(),
                Selection | Cursor | TitleBar | StatusBar => Style::new().reversed(),
                _ => Style::new(),
            };
        }
        let (
            bg,
            surface,
            selection,
            text,
            muted,
            technical,
            accent,
            participant,
            target,
            success,
            warning,
            error,
        ) = match self {
            // Gruvbox dark medium from Tinted Theming schemes (MIT), commit
            // fdca32a0d14ec80ad83a78a9ccb85592ca6cb9e1, base16/gruvbox-dark-medium.yaml.
            // License notice: assets/themes/LICENSE.
            // Palette authors: Dawid Kurek and morhetz (github.com/morhetz/gruvbox).
            // Muted uses base04 for readability on both screen and human surface.
            Self::Gruvbox => (
                Color::Rgb(40, 40, 40),
                Color::Rgb(60, 56, 54),
                Color::Rgb(80, 73, 69),
                Color::Rgb(213, 196, 161),
                Color::Rgb(189, 174, 147),
                Color::Rgb(235, 219, 178),
                Color::Rgb(131, 165, 152),
                Color::Rgb(211, 134, 155),
                Color::Rgb(254, 128, 25),
                Color::Rgb(184, 187, 38),
                Color::Rgb(250, 189, 47),
                Color::Rgb(251, 73, 52),
            ),
            Self::Terminal => (
                Color::Black,
                Color::Indexed(236),
                Color::DarkGray,
                Color::Gray,
                Color::Gray,
                Color::White,
                Color::LightCyan,
                Color::LightMagenta,
                Color::Indexed(208),
                Color::LightGreen,
                Color::LightYellow,
                Color::LightRed,
            ),
            Self::NoColor => unreachable!(),
        };
        match role {
            Screen => Style::new().fg(text).bg(bg),
            Text => Style::new().fg(text),
            HumanSurface => Style::new().fg(text).bg(surface),
            Muted => Style::new().fg(muted),
            Technical => Style::new().fg(technical),
            Heading => Style::new().fg(accent).bold(),
            Accent | Border | Logo | OwnAgent => Style::new().fg(accent),
            Human | OtherAgent => Style::new().fg(participant),
            Target => Style::new().fg(target),
            KeyHint => Style::new().fg(participant).italic(),
            Hotkey | Warning => Style::new().fg(warning),
            Success => Style::new().fg(success),
            Error => Style::new().fg(error),
            Selection => Style::new().fg(text).bg(selection).bold(),
            Cursor => Style::new().fg(bg).bg(text),
            TitleBar | StatusBar => Style::new().fg(bg).bg(accent).bold(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn composer_roles_inherit_surface_and_keep_non_color_cues() {
        for scheme in testing::SCHEMES {
            let target = scheme.style(Role::Target);
            let hint = scheme.style(Role::KeyHint);
            assert!(target.bg.is_none() && hint.bg.is_none());
            assert!(hint.add_modifier.contains(ratatui::style::Modifier::ITALIC));
            if scheme != Scheme::NoColor {
                assert_ne!(target.fg, hint.fg);
                assert_ne!(hint.fg, scheme.style(Role::Text).fg);
            }
        }
        assert_eq!(
            Scheme::Gruvbox.style(Role::Target).fg,
            Some(Color::Rgb(254, 128, 25))
        );
    }

    #[test]
    fn selector_precedence_and_validation() {
        assert_eq!(resolve(None, None, false).unwrap(), Scheme::Gruvbox);
        assert_eq!(resolve(None, Some(""), true).unwrap(), Scheme::NoColor);
        assert_eq!(
            resolve(None, Some("terminal"), true).unwrap(),
            Scheme::Terminal
        );
        assert_eq!(
            resolve(Some("gruvbox-dark-medium"), Some("bad"), true).unwrap(),
            Scheme::Gruvbox
        );
        assert!(resolve(None, Some("bad"), false).is_err());
        assert!(resolve(Some(""), None, false).is_err());
    }
}

/// Thread-local fixtures keep scheme coverage independent of parallel tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::Cell;
    thread_local! { pub static OVERRIDE: Cell<Option<Scheme>> = const { Cell::new(None) }; }
    pub fn with_scheme(scheme: Scheme, f: impl FnOnce()) {
        struct Restore(Option<Scheme>);
        impl Drop for Restore {
            fn drop(&mut self) {
                OVERRIDE.set(self.0);
            }
        }
        let _restore = Restore(OVERRIDE.replace(Some(scheme)));
        f();
    }
    pub const SCHEMES: [Scheme; 3] = [Scheme::Gruvbox, Scheme::Terminal, Scheme::NoColor];
    pub fn assert_buffer(buffer: &ratatui::buffer::Buffer) {
        use Role::*;
        let roles = [
            Screen,
            Text,
            Muted,
            Technical,
            Heading,
            Accent,
            HumanSurface,
            OwnAgent,
            OtherAgent,
            Human,
            Hotkey,
            Target,
            KeyHint,
            Border,
            Selection,
            Cursor,
            Success,
            Warning,
            Error,
            TitleBar,
            StatusBar,
            Logo,
        ];
        let colors: Vec<_> = roles
            .into_iter()
            .flat_map(|role| {
                let s = style(role);
                [s.fg, s.bg].into_iter().flatten()
            })
            .collect();
        let mut covered = 0;
        for (i, cell) in buffer.content.iter().enumerate() {
            // Ratatui resets cells hidden by a wide grapheme; those cells are
            // not emitted by its terminal backend.
            if i % usize::from(buffer.area.width) == 0 {
                covered = 0;
            }
            if covered > 0 {
                covered -= 1;
                continue;
            }
            covered = unicode_width::UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
            assert!(
                colors.contains(&cell.fg),
                "cell {i}: unexpected foreground {:?}",
                cell.fg
            );
            assert!(
                colors.contains(&cell.bg),
                "cell {i}: unexpected background {:?}",
                cell.bg
            );
        }
    }
}
