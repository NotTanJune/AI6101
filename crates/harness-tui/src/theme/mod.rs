//! Colour theme: one dark scheme of named semantic colours, plus terminal colour-depth support.
//!
//! Every colour in the UI comes from a [`Theme`]. Colours are stored as [`Rgb`] and turned into
//! a ratatui [`Color`] by [`Theme::resolve`], which depends on the terminal's [`ColorSupport`]:
//! true colour terminals get exact RGB, 256-colour terminals the nearest palette entry and
//! basic terminals the closest of the 16 named colours. When `NO_COLOR` is set the theme
//! resolves everything to [`Color::Reset`] and the UI relies on modifiers only.
//!
//! Detection ([`ColorSupport::detect`], [`Theme::detect`]) is a pure function of an environment
//! lookup closure so it can be tested without touching the process environment.

use ratatui::style::{Color, Modifier, Style};

pub mod builtin;
pub mod config;
pub mod palette;

/// An RGB colour independent of the terminal's colour depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// How many colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    /// 24-bit colour.
    TrueColor,
    /// The 256-colour xterm palette.
    Indexed256,
    /// The 16 named ANSI colours.
    Basic,
}

impl ColorSupport {
    /// Detect colour depth from `COLORTERM` and `TERM` (looked up with `env`).
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Self::TrueColor;
        }
        let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
        if term.contains("truecolor") || term.contains("24bit") || term.contains("direct") {
            Self::TrueColor
        } else if term.contains("256color") {
            Self::Indexed256
        } else {
            Self::Basic
        }
    }
}

/// Blend `a` toward `b`: `t == 0.0` is `a`, `t == 1.0` is `b`. `t` is clamped to `0..=1`.
pub fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    let mix = |x: u8, y: u8| -> u8 {
        let value = f32::from(x) + (f32::from(y) - f32::from(x)) * t;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            value.round().clamp(0.0, 255.0) as u8
        }
    };
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Move `color` toward `toward` by `1.0 - factor`: a `factor` of `1.0` keeps the colour and
/// `0.0` gives `toward`. Used for backdrops and fades.
pub fn dim(color: Rgb, toward: Rgb, factor: f32) -> Rgb {
    lerp(color, toward, 1.0 - factor)
}

/// The sixteen ANSI colours with their usual xterm values.
const BASIC: [(Color, Rgb); 16] = [
    (Color::Black, Rgb(0, 0, 0)),
    (Color::Red, Rgb(205, 0, 0)),
    (Color::Green, Rgb(0, 205, 0)),
    (Color::Yellow, Rgb(205, 205, 0)),
    (Color::Blue, Rgb(0, 0, 238)),
    (Color::Magenta, Rgb(205, 0, 205)),
    (Color::Cyan, Rgb(0, 205, 205)),
    (Color::Gray, Rgb(229, 229, 229)),
    (Color::DarkGray, Rgb(127, 127, 127)),
    (Color::LightRed, Rgb(255, 0, 0)),
    (Color::LightGreen, Rgb(0, 255, 0)),
    (Color::LightYellow, Rgb(255, 255, 0)),
    (Color::LightBlue, Rgb(92, 92, 255)),
    (Color::LightMagenta, Rgb(255, 0, 255)),
    (Color::LightCyan, Rgb(0, 255, 255)),
    (Color::White, Rgb(255, 255, 255)),
];

fn distance(a: Rgb, b: Rgb) -> u32 {
    let d = |x: u8, y: u8| u32::from(x.abs_diff(y)).pow(2);
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn nearest_basic(rgb: Rgb) -> Color {
    BASIC
        .iter()
        .min_by_key(|(_, value)| distance(rgb, *value))
        .map_or(Color::Reset, |(color, _)| *color)
}

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn nearest_cube_level(value: u8) -> usize {
    (0..CUBE_LEVELS.len())
        .min_by_key(|&i| value.abs_diff(CUBE_LEVELS[i]))
        .unwrap_or(0)
}

fn nearest_indexed(rgb: Rgb) -> Color {
    let (r, g, b) = (
        nearest_cube_level(rgb.0),
        nearest_cube_level(rgb.1),
        nearest_cube_level(rgb.2),
    );
    let cube = Rgb(CUBE_LEVELS[r], CUBE_LEVELS[g], CUBE_LEVELS[b]);
    let cube_index = 16 + 36 * r + 6 * g + b;
    // Greys: 232..=255 are 8, 18, ..., 238.
    let average = (u32::from(rgb.0) + u32::from(rgb.1) + u32::from(rgb.2)) / 3;
    let grey_step = ((average.saturating_sub(8) + 5) / 10).min(23);
    let grey_value = u8::try_from(8 + 10 * grey_step).unwrap_or(238);
    let grey = Rgb(grey_value, grey_value, grey_value);
    let index = if distance(rgb, grey) < distance(rgb, cube) {
        232 + usize::try_from(grey_step).unwrap_or(0)
    } else {
        cube_index
    };
    Color::Indexed(u8::try_from(index).unwrap_or(15))
}

/// Best-effort conversion of a ratatui colour back to RGB (for blending cells that were already
/// resolved). `Reset` has no RGB value and returns `None`.
pub fn color_to_rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Rgb(r, g, b) => Some(Rgb(r, g, b)),
        Color::Indexed(i) => Some(indexed_to_rgb(i)),
        Color::Reset => None,
        named => BASIC.iter().find(|(c, _)| *c == named).map(|(_, v)| *v),
    }
}

fn indexed_to_rgb(index: u8) -> Rgb {
    match index {
        0..=15 => BASIC[usize::from(index)].1,
        16..=231 => {
            let n = usize::from(index - 16);
            Rgb(
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            Rgb(v, v, v)
        }
    }
}

/// The colour scheme. Every field is a semantic role, not a hue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Colour depth the theme resolves to.
    pub support: ColorSupport,
    /// `NO_COLOR` is set: resolve to the terminal defaults and use modifiers only.
    pub no_color: bool,
    /// Frame background.
    pub bg: Rgb,
    /// Slightly lifted background (input box, status bar).
    pub surface: Rgb,
    /// Raised background (popups, tool cards).
    pub raised: Rgb,
    /// Selected row background.
    pub highlight: Rgb,
    /// Idle border.
    pub border: Rgb,
    /// Body text.
    pub text: Rgb,
    /// Secondary text.
    pub muted: Rgb,
    /// Primary accent (focus, titles, selection).
    pub accent: Rgb,
    /// Second end of the accent gradient (spinner, shimmer).
    pub accent_alt: Rgb,
    /// User messages.
    pub user: Rgb,
    /// Assistant gutter and labels.
    pub assistant: Rgb,
    /// Success.
    pub success: Rgb,
    /// Warning.
    pub warning: Rgb,
    /// Error.
    pub error: Rgb,
    /// Background tint of added diff lines.
    pub diff_add_bg: Rgb,
    /// Background tint of removed diff lines.
    pub diff_del_bg: Rgb,
}

impl Default for Theme {
    /// The dark scheme on a true colour terminal.
    fn default() -> Self {
        Self {
            support: ColorSupport::TrueColor,
            no_color: false,
            bg: Rgb(0x1a, 0x1b, 0x26),
            surface: Rgb(0x1f, 0x23, 0x35),
            raised: Rgb(0x24, 0x28, 0x3b),
            highlight: Rgb(0x33, 0x3a, 0x57),
            border: Rgb(0x3b, 0x42, 0x61),
            text: Rgb(0xc0, 0xca, 0xf5),
            muted: Rgb(0x6c, 0x75, 0xa0),
            accent: Rgb(0x7d, 0xcf, 0xff),
            accent_alt: Rgb(0xbb, 0x9a, 0xf7),
            user: Rgb(0x7a, 0xa2, 0xf7),
            assistant: Rgb(0xbb, 0x9a, 0xf7),
            success: Rgb(0x9e, 0xce, 0x6a),
            warning: Rgb(0xe0, 0xaf, 0x68),
            error: Rgb(0xf7, 0x76, 0x8e),
            diff_add_bg: Rgb(0x20, 0x30, 0x3b),
            diff_del_bg: Rgb(0x37, 0x22, 0x2c),
        }
    }
}

impl Theme {
    /// The dark scheme for the terminal described by `env` (`COLORTERM`, `TERM`, `NO_COLOR`).
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Self {
        Self::default().with_terminal(env)
    }

    /// Keep every colour of this theme and set only `support` and `no_color` from the
    /// terminal described by `env`.
    pub fn with_terminal(self, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let no_color = env("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self {
            support: ColorSupport::detect(env),
            no_color,
            ..self
        }
    }

    /// [`Theme::detect`] against the real process environment.
    pub fn from_process_env() -> Self {
        Self::detect(&|name| std::env::var(name).ok())
    }

    /// Terminal colour for `rgb` under this theme's colour support.
    pub fn resolve(&self, rgb: Rgb) -> Color {
        if self.no_color {
            return Color::Reset;
        }
        match self.support {
            ColorSupport::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            ColorSupport::Indexed256 => nearest_indexed(rgb),
            ColorSupport::Basic => nearest_basic(rgb),
        }
    }

    /// Foreground style.
    pub fn fg(&self, rgb: Rgb) -> Style {
        Style::new().fg(self.resolve(rgb))
    }

    /// Foreground on a background.
    pub fn on(&self, fg: Rgb, bg: Rgb) -> Style {
        Style::new().fg(self.resolve(fg)).bg(self.resolve(bg))
    }

    /// Background style.
    pub fn bg_style(&self, rgb: Rgb) -> Style {
        Style::new().bg(self.resolve(rgb))
    }

    /// Body text on the frame background.
    pub fn base(&self) -> Style {
        self.on(self.text, self.bg)
    }

    /// Secondary text. Adds `DIM` when there are no colours to tell it apart.
    pub fn muted_style(&self) -> Style {
        let style = self.fg(self.muted);
        if self.no_color {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    /// Bold accent text.
    pub fn accent_style(&self) -> Style {
        self.fg(self.accent).add_modifier(Modifier::BOLD)
    }

    /// Selected row: highlight background and bold accent text. Without colours, bold and
    /// underlined (never reversed).
    pub fn selected_style(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            self.on(self.accent, self.highlight)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Blend `a` toward `b` (see [`lerp`]).
    pub fn mix(&self, a: Rgb, b: Rgb, t: f32) -> Rgb {
        lerp(a, b, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn colorterm_truecolor_and_24bit_mean_true_colour() {
        for value in ["truecolor", "24bit", "TrueColor"] {
            let env = env_of(&[("COLORTERM", value), ("TERM", "xterm")]);
            assert_eq!(ColorSupport::detect(&env), ColorSupport::TrueColor);
        }
    }

    #[test]
    fn term_decides_when_colorterm_is_absent() {
        assert_eq!(
            ColorSupport::detect(&env_of(&[("TERM", "xterm-256color")])),
            ColorSupport::Indexed256
        );
        assert_eq!(
            ColorSupport::detect(&env_of(&[("TERM", "xterm-direct")])),
            ColorSupport::TrueColor
        );
        assert_eq!(
            ColorSupport::detect(&env_of(&[("TERM", "linux")])),
            ColorSupport::Basic
        );
        assert_eq!(ColorSupport::detect(&env_of(&[])), ColorSupport::Basic);
    }

    #[test]
    fn no_color_needs_a_non_empty_value() {
        let set = Theme::detect(&env_of(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]));
        assert!(set.no_color);
        assert_eq!(set.resolve(set.accent), Color::Reset);
        let empty = Theme::detect(&env_of(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")]));
        assert!(!empty.no_color);
        assert_eq!(empty.resolve(empty.accent), Color::Rgb(0x7d, 0xcf, 0xff));
    }

    #[test]
    fn resolve_maps_rgb_per_colour_support() {
        let mut theme = Theme::default();
        assert_eq!(theme.resolve(Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        theme.support = ColorSupport::Indexed256;
        // Pure red is cube entry 16 + 36*5.
        assert_eq!(theme.resolve(Rgb(255, 0, 0)), Color::Indexed(196));
        // Mid grey picks the greyscale ramp.
        assert_eq!(theme.resolve(Rgb(128, 128, 128)), Color::Indexed(244));
        theme.support = ColorSupport::Basic;
        assert_eq!(theme.resolve(Rgb(250, 10, 10)), Color::LightRed);
        assert_eq!(theme.resolve(Rgb(0, 0, 0)), Color::Black);
        assert_eq!(theme.resolve(Rgb(255, 255, 255)), Color::White);
    }

    #[test]
    fn lerp_blends_and_clamps() {
        let a = Rgb(0, 0, 0);
        let b = Rgb(200, 100, 50);
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        assert_eq!(lerp(a, b, 0.5), Rgb(100, 50, 25));
        assert_eq!(lerp(a, b, 7.0), b);
        assert_eq!(lerp(a, b, -1.0), a);
        assert_eq!(lerp(a, b, f32::NAN), a);
    }

    #[test]
    fn dim_moves_toward_the_target() {
        let bg = Rgb(10, 10, 10);
        let c = Rgb(210, 110, 10);
        assert_eq!(dim(c, bg, 1.0), c);
        assert_eq!(dim(c, bg, 0.0), bg);
        let half = dim(c, bg, 0.5);
        assert!(half.0 < c.0 && half.0 > bg.0);
    }

    #[test]
    fn colours_round_trip_through_rgb() {
        assert_eq!(color_to_rgb(Color::Rgb(1, 2, 3)), Some(Rgb(1, 2, 3)));
        assert_eq!(color_to_rgb(Color::Indexed(196)), Some(Rgb(255, 0, 0)));
        assert_eq!(color_to_rgb(Color::Indexed(244)), Some(Rgb(128, 128, 128)));
        assert_eq!(color_to_rgb(Color::White), Some(Rgb(255, 255, 255)));
        assert_eq!(color_to_rgb(Color::Reset), None);
    }

    #[test]
    fn the_selected_style_never_uses_reverse() {
        for no_color in [false, true] {
            let theme = Theme {
                no_color,
                ..Theme::default()
            };
            assert!(
                !theme
                    .selected_style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn with_terminal_keeps_every_colour_and_sets_only_terminal_fields() {
        let base = Theme::default();
        let env = env_of(&[("COLORTERM", "truecolor")]);
        let changed = base.clone().with_terminal(&env);
        assert_eq!(changed, base);

        let plain = Theme {
            support: ColorSupport::Basic,
            ..Theme::default()
        };
        let restored = plain.with_terminal(&env_of(&[("COLORTERM", "truecolor")]));
        assert_eq!(restored.support, ColorSupport::TrueColor);
        assert_eq!(restored.bg, Theme::default().bg);
        assert_eq!(restored.accent, Theme::default().accent);
    }

    #[test]
    fn no_color_resolves_every_colour_to_reset_after_with_terminal() {
        let theme = Theme::default().with_terminal(&env_of(&[("NO_COLOR", "1")]));
        assert!(theme.no_color);
        assert_eq!(theme.resolve(theme.bg), Color::Reset);
        assert_eq!(theme.resolve(theme.accent), Color::Reset);
    }
}
