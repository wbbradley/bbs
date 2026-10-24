//! Semantic styles shared by every terminal renderer; connect can switch live.

use anyhow::{Result, bail};
use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Components choose a purpose, never a palette color. Foreground-only roles
/// inherit their enclosing surface, including human input and outgoing rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Screen,
    Text,
    Muted,
    Technical,
    Heading,
    Accent,
    HumanSurface,
    /// A quiet note on the human surface, a little lighter than it.
    HumanNote,
    /// Conversation mail between participants other than the viewing human.
    OtherConversation,
    OtherConversationHeader,
    GroupConversation,
    GroupConversationHeader,
    Agent,
    Human,
    Group,
    Timestamp,
    Tool,
    Hotkey,
    /// Keyboard guidance, distinct from message text.
    KeyHint,
    Selection,
    Cursor,
    Success,
    /// Turn boundaries: ready to run, and finished running.
    Go,
    Stop,
    Warning,
    Error,
    TitleBar,
    StatusBar,
    Logo,
    /// A card that needs the human: a background and its title bar.
    WarningSurface,
    WarningTitle,
    /// Text matching what the human typed to find it.
    SearchHit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scheme {
    #[default]
    #[serde(rename = "gruvbox-dark-medium")]
    Gruvbox,
    Terminal,
    NoColor,
}

static SCHEME: RwLock<Scheme> = RwLock::new(Scheme::Gruvbox);

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

/// Resolve startup options; CLI, BBS_THEME and NO_COLOR override saved preferences.
pub fn init(cli: Option<&str>) -> Result<bool> {
    let env = std::env::var("BBS_THEME").ok();
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    init_options(cli, env.as_deref(), no_color)
}

fn init_options(cli: Option<&str>, env: Option<&str>, no_color: bool) -> Result<bool> {
    select(resolve(cli, env, no_color)?);
    Ok(cli.is_some() || env.is_some_and(|s| !s.is_empty()) || no_color)
}

pub fn current() -> Scheme {
    #[cfg(test)]
    if let Some(scheme) = testing::OVERRIDE.get() {
        return scheme;
    }
    *SCHEME.read().expect("theme lock")
}

/// Change appearance without changing any application data or navigation state.
pub fn select(scheme: Scheme) {
    #[cfg(test)]
    if testing::OVERRIDE.get().is_some() {
        testing::OVERRIDE.set(Some(scheme));
        return;
    }
    *SCHEME.write().expect("theme lock") = scheme;
}

pub fn style(role: Role) -> Style {
    current().style(role)
}

/// Preserve the surface while keeping a categorical foreground visible on it.
pub fn on_surface(surface: Style, category: Style) -> Style {
    let mut combined = surface.patch(category);
    if current() != Scheme::NoColor && category.fg.is_some() && category.fg == surface.bg {
        combined.fg = surface.fg;
        combined = combined.underlined();
    }
    combined
}

impl Scheme {
    pub const ALL: [Self; 3] = [Self::Gruvbox, Self::Terminal, Self::NoColor];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Gruvbox => "Gruvbox dark medium",
            Self::Terminal => "Terminal palette",
            Self::NoColor => "No color",
        }
    }

    pub fn style(self, role: Role) -> Style {
        use Role::*;
        if self == Self::NoColor {
            return match role {
                Screen => Style::new().fg(Color::Reset).bg(Color::Reset),
                Muted | Timestamp | HumanNote => Style::new().dim(),
                KeyHint => Style::new().italic(),
                Heading | Agent | Human | Group | Hotkey | Success | Warning | Error | Go
                | Stop => Style::new().bold(),
                Selection | Cursor | TitleBar | StatusBar => Style::new().reversed(),
                WarningTitle => Style::new().reversed().bold(),
                SearchHit => Style::new().bold().underlined(),
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
            HumanNote => Style::new().fg(match self {
                Self::Gruvbox => tint(surface, text, 25),
                Self::Terminal => Color::Indexed(240),
                Self::NoColor => unreachable!(),
            }),
            OtherConversation => Style::new().fg(text).bg(match self {
                Self::Gruvbox => Color::Rgb(44, 54, 52),
                Self::Terminal => Color::Indexed(235),
                Self::NoColor => unreachable!(),
            }),
            OtherConversationHeader => Style::new().fg(text).bg(match self {
                Self::Gruvbox => Color::Rgb(43, 65, 59),
                Self::Terminal => Color::Indexed(237),
                Self::NoColor => unreachable!(),
            }),
            GroupConversation | GroupConversationHeader => {
                let header = role == GroupConversationHeader;
                Style::new().fg(text).bg(match self {
                    // Gruvbox orange (base09), blended into the scheme's background.
                    // More of the same accent makes the header stronger without
                    // changing the body/header hue or their text contrast abruptly.
                    Self::Gruvbox => {
                        tint(bg, Color::Rgb(254, 128, 25), if header { 18 } else { 10 })
                    }
                    // ANSI colors have terminal-defined RGB values. Keep palette
                    // references rather than pretending we can transform those RGBs.
                    Self::Terminal => Color::Indexed(if header { 88 } else { 52 }),
                    Self::NoColor => unreachable!(),
                })
            }
            Muted | Timestamp => Style::new().fg(muted),
            Technical | Tool => Style::new().fg(technical),
            Heading => Style::new().fg(accent).bold(),
            Logo if self == Self::Gruvbox => Style::new().fg(muted),
            Accent | Logo | Agent => Style::new().fg(accent),
            Human => Style::new().fg(participant),
            KeyHint => Style::new().fg(participant).italic(),
            Hotkey | Warning | Group => Style::new().fg(warning),
            Success | Go => Style::new().fg(success),
            Error | Stop => Style::new().fg(error),
            Selection => Style::new().fg(text).bg(selection).bold(),
            Cursor => Style::new().fg(bg).bg(text),
            TitleBar | StatusBar if self == Self::Gruvbox => {
                Style::new().fg(text).bg(selection).bold()
            }
            TitleBar | StatusBar => Style::new().fg(bg).bg(accent).bold(),
            WarningSurface => Style::new().fg(text).bg(match self {
                Self::Gruvbox => tint(bg, warning, 18),
                Self::Terminal => Color::Indexed(58),
                Self::NoColor => unreachable!(),
            }),
            WarningTitle => Style::new().fg(bg).bg(warning).bold(),
            SearchHit => Style::new()
                .fg(match self {
                    // Gruvbox orange (base09).
                    Self::Gruvbox => Color::Rgb(254, 128, 25),
                    Self::Terminal => Color::Indexed(208),
                    Self::NoColor => unreachable!(),
                })
                .bold(),
        }
    }
}

/// Mix a palette accent into its RGB background by a percentage.
fn tint(background: Color, accent: Color, percent: u16) -> Color {
    let (Color::Rgb(r, g, b), Color::Rgb(ar, ag, ab)) = (background, accent) else {
        return background;
    };
    let mix = |base: u8, ink: u8| {
        ((u16::from(base) * (100 - percent) + u16::from(ink) * percent) / 100) as u8
    };
    Color::Rgb(mix(r, ar), mix(g, ag), mix(b, ab))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_surfaces_are_warm_and_the_header_has_more_accent() {
        let body = Scheme::Gruvbox.style(Role::GroupConversation).bg.unwrap();
        let header = Scheme::Gruvbox
            .style(Role::GroupConversationHeader)
            .bg
            .unwrap();
        let Color::Rgb(r, g, b) = body else {
            panic!("RGB scheme")
        };
        let Color::Rgb(hr, hg, hb) = header else {
            panic!("RGB scheme")
        };
        assert!(r > g && g > b);
        assert!(hr > hg && hg > hb && hr - hb > r - b);
        assert_eq!(
            body,
            tint(
                Scheme::Gruvbox.style(Role::Screen).bg.unwrap(),
                Color::Rgb(254, 128, 25),
                10
            )
        );
        assert_ne!(
            body,
            Scheme::Gruvbox.style(Role::OtherConversation).bg.unwrap()
        );
        for role in [Role::GroupConversation, Role::GroupConversationHeader] {
            assert_eq!(Scheme::NoColor.style(role), Style::default());
            assert!(matches!(
                Scheme::Terminal.style(role).bg,
                Some(Color::Indexed(_))
            ));
        }
    }

    #[test]
    fn categorical_roles_inherit_surfaces_and_bar_collisions_stay_visible() {
        for scheme in testing::SCHEMES {
            testing::with_scheme(scheme, || {
                for role in [
                    Role::Agent,
                    Role::Human,
                    Role::Group,
                    Role::Timestamp,
                    Role::Tool,
                    Role::SearchHit,
                ] {
                    let category = style(role);
                    assert!(category.bg.is_none());
                    for surface in [
                        Role::Screen,
                        Role::HumanSurface,
                        Role::OtherConversation,
                        Role::OtherConversationHeader,
                        Role::GroupConversation,
                        Role::GroupConversationHeader,
                        Role::Selection,
                        Role::TitleBar,
                        Role::StatusBar,
                        Role::WarningSurface,
                        Role::WarningTitle,
                    ] {
                        let bar = style(surface);
                        let combined = on_surface(bar, category);
                        assert_eq!(combined.bg, bar.bg);
                        assert!(combined.add_modifier.contains(bar.add_modifier));
                        if scheme != Scheme::NoColor {
                            assert_ne!(combined.fg, combined.bg);
                        }
                    }
                }
                if scheme != Scheme::NoColor {
                    assert_ne!(style(Role::Agent).fg, style(Role::Human).fg);
                    assert_ne!(style(Role::Agent).fg, style(Role::Group).fg);
                }
                if scheme == Scheme::Terminal {
                    let bar = style(Role::TitleBar);
                    let agent = on_surface(bar, style(Role::Agent));
                    assert_eq!(agent.fg, bar.fg);
                    assert!(
                        agent
                            .add_modifier
                            .contains(ratatui::style::Modifier::UNDERLINED)
                    );
                }
            });
        }
    }

    #[test]
    fn the_warning_card_stands_apart_from_every_other_surface() {
        for scheme in [Scheme::Gruvbox, Scheme::Terminal] {
            let card = scheme.style(Role::WarningSurface).bg.unwrap();
            let title = scheme.style(Role::WarningTitle);
            for other in [
                Role::Screen,
                Role::HumanSurface,
                Role::OtherConversation,
                Role::GroupConversation,
                Role::Selection,
                Role::TitleBar,
            ] {
                assert_ne!(Some(card), scheme.style(other).bg, "{scheme:?} {other:?}");
                assert_ne!(title.bg, scheme.style(other).bg, "{scheme:?} {other:?}");
            }
            assert_ne!(title.bg, Some(card));
            // The title reads in the warning colour itself, text in the
            // background's: the scheme's own warning, no new palette entry.
            assert_eq!(title.bg, scheme.style(Role::Warning).fg);
            assert_eq!(title.fg, scheme.style(Role::Screen).bg);
            assert!(title.add_modifier.contains(ratatui::style::Modifier::BOLD));
            assert_eq!(
                scheme.style(Role::WarningSurface).fg,
                scheme.style(Role::Text).fg
            );
        }
        let plain = Scheme::NoColor.style(Role::WarningTitle);
        assert!(
            plain
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED | ratatui::style::Modifier::BOLD)
        );
        assert_eq!(Scheme::NoColor.style(Role::WarningSurface), Style::new());
    }

    #[test]
    fn key_hints_inherit_surface_and_keep_non_color_cues() {
        for scheme in testing::SCHEMES {
            let hint = scheme.style(Role::KeyHint);
            assert!(hint.bg.is_none());
            assert!(hint.add_modifier.contains(ratatui::style::Modifier::ITALIC));
            if scheme != Scheme::NoColor {
                assert_ne!(hint.fg, scheme.style(Role::Text).fg);
            }
        }
    }

    #[test]
    fn gruvbox_bars_and_logo_use_quiet_existing_roles() {
        for role in [Role::TitleBar, Role::StatusBar] {
            let bar = Scheme::Gruvbox.style(role);
            assert_eq!(bar.fg, Scheme::Gruvbox.style(Role::Text).fg);
            assert_eq!(bar.bg, Scheme::Gruvbox.style(Role::Selection).bg);
            assert!(bar.add_modifier.contains(ratatui::style::Modifier::BOLD));
            let terminal = Scheme::Terminal.style(role);
            assert_eq!(terminal.fg, Scheme::Terminal.style(Role::Screen).bg);
            assert_eq!(terminal.bg, Scheme::Terminal.style(Role::Accent).fg);
            assert_eq!(Scheme::NoColor.style(role), Style::new().reversed());
        }
        assert_eq!(
            Scheme::Gruvbox.style(Role::Logo),
            Scheme::Gruvbox.style(Role::Muted)
        );
        assert_eq!(
            Scheme::Terminal.style(Role::Logo),
            Scheme::Terminal.style(Role::Accent)
        );
        assert_eq!(Scheme::NoColor.style(Role::Logo), Style::new());
    }

    #[test]
    fn startup_opt_out_blocks_saved_theme_but_allows_live_selection() {
        testing::with_scheme(Scheme::Gruvbox, || {
            for env in [None, Some("")] {
                assert!(init_options(None, env, true).unwrap());
                assert_eq!(current(), Scheme::NoColor);
                select(Scheme::Terminal);
                assert_eq!(current(), Scheme::Terminal);
                assert!(!init_options(None, env, false).unwrap());
                assert_eq!(current(), Scheme::Gruvbox);
            }
            assert!(init_options(None, Some("terminal"), true).unwrap());
            assert_eq!(current(), Scheme::Terminal);
            assert!(init_options(Some("gruvbox-dark-medium"), Some("terminal"), true).unwrap());
            assert_eq!(current(), Scheme::Gruvbox);
        });
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
    pub const SCHEMES: [Scheme; 3] = Scheme::ALL;
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
            HumanNote,
            OtherConversation,
            OtherConversationHeader,
            GroupConversation,
            GroupConversationHeader,
            Agent,
            Human,
            Group,
            Timestamp,
            Tool,
            Hotkey,
            KeyHint,
            Selection,
            Cursor,
            Success,
            Warning,
            Error,
            TitleBar,
            StatusBar,
            Logo,
            WarningSurface,
            WarningTitle,
            SearchHit,
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
