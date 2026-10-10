//! Base palettes and the derivation of semantic theme roles.
//!
//! A Ghostty theme file defines only a base palette (background, foreground, the sixteen ANSI
//! colours and an optional selection background). [`derive`] turns that base into every
//! semantic [`Theme`] role with one fixed set of rules, so built-in and user supplied themes
//! look consistent. [`parse_hex`] and [`parse_ghostty`] read the text formats.

use super::{ColorSupport, Rgb, Theme, lerp};

/// Minimum WCAG contrast ratio for text-like roles against the background.
pub const MIN_ROLE_CONTRAST: f32 = 3.0;

/// The base palette a theme file defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasePalette {
    pub background: Rgb,
    pub foreground: Rgb,
    pub palette: [Rgb; 16],
    pub selection_background: Option<Rgb>,
}

/// Parse `#rrggbb`, `rrggbb`, `#rgb` or `rgb` (case-insensitive). `#rgb` expands each digit.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let digits = s.strip_prefix('#').unwrap_or(s);
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let nibble = |i: usize| {
        u8::from_str_radix(&digits[i..i + 1], 16)
            .ok()
            .map(|d| d * 17)
    };
    match digits.len() {
        6 => Some(Rgb(value(0)?, value(2)?, value(4)?)),
        3 => Some(Rgb(nibble(0)?, nibble(1)?, nibble(2)?)),
        _ => None,
    }
}

/// Parse a Ghostty-format theme file.
///
/// Blank lines and lines starting with `#` are ignored, as are keys other than the ones used
/// here (for example `cursor-color`). Errors name the 1-based line number, or the missing key.
pub fn parse_ghostty(text: &str) -> Result<BasePalette, String> {
    let mut background = None;
    let mut foreground = None;
    let mut selection_background = None;
    let mut palette: [Option<Rgb>; 16] = [None; 16];

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let bad = |what: &str| format!("line {line_no}: invalid {what} {value:?}");
        match key {
            "background" => {
                background = Some(parse_hex(value).ok_or_else(|| bad("background colour"))?);
            }
            "foreground" => {
                foreground = Some(parse_hex(value).ok_or_else(|| bad("foreground colour"))?);
            }
            "selection-background" => {
                selection_background =
                    Some(parse_hex(value).ok_or_else(|| bad("selection-background colour"))?);
            }
            "palette" => {
                let (index_text, hex) = value
                    .split_once('=')
                    .ok_or_else(|| format!("line {line_no}: palette entry must be N=#hex"))?;
                let slot: usize = index_text
                    .trim()
                    .parse()
                    .ok()
                    .filter(|n| *n < 16)
                    .ok_or_else(|| format!("line {line_no}: palette index must be 0..=15"))?;
                palette[slot] =
                    Some(parse_hex(hex.trim()).ok_or_else(|| {
                        format!("line {line_no}: invalid palette colour {hex:?}")
                    })?);
            }
            _ => {}
        }
    }

    let background = background.ok_or("missing `background`")?;
    let foreground = foreground.ok_or("missing `foreground`")?;
    let mut resolved = [Rgb(0, 0, 0); 16];
    for (slot, entry) in palette.iter().enumerate() {
        resolved[slot] = entry.ok_or_else(|| format!("missing `palette = {slot}`"))?;
    }
    Ok(BasePalette {
        background,
        foreground,
        palette: resolved,
        selection_background,
    })
}

/// WCAG 2.x relative luminance of `c`, `0.0` (black) to `1.0` (white).
pub fn luminance(c: Rgb) -> f32 {
    let linear = |v: u8| {
        let x = f32::from(v) / 255.0;
        if x <= 0.040_45 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(c.0) + 0.7152 * linear(c.1) + 0.0722 * linear(c.2)
}

/// WCAG 2.x contrast ratio between two colours, `1.0` (identical) to `21.0` (black on white).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Move `c` toward `toward` in steps of `0.05` until its contrast against `bg` is at least
/// `min`. A colour that already passes is returned unchanged. If no step passes, the result
/// is `toward` itself.
pub fn ensure_contrast(c: Rgb, bg: Rgb, toward: Rgb, min: f32) -> Rgb {
    if contrast(c, bg) >= min {
        return c;
    }
    for step in 1..=20 {
        let candidate = lerp(c, toward, step as f32 * 0.05);
        if contrast(candidate, bg) >= min {
            return candidate;
        }
    }
    toward
}

/// Minimum contrast for the body text role against the background.
pub const MIN_TEXT_CONTRAST: f32 = 4.5;

/// The body text colour: the base foreground, moved toward black on light backgrounds or
/// toward white on dark ones until it reaches [`MIN_TEXT_CONTRAST`].
pub fn readable_text(foreground: Rgb, bg: Rgb) -> Rgb {
    let toward = if luminance(bg) >= 0.5 {
        Rgb(0, 0, 0)
    } else {
        Rgb(255, 255, 255)
    };
    ensure_contrast(foreground, bg, toward, MIN_TEXT_CONTRAST)
}

/// Derive every semantic role from a base palette (see the design table in the plan).
///
/// The result is true colour with `no_color: false`; the caller applies the terminal's
/// support with [`Theme::with_terminal`].
pub fn derive(base: &BasePalette) -> Theme {
    let bg = base.background;
    let text = readable_text(base.foreground, bg);
    let p = &base.palette;
    let highlight = match base.selection_background {
        Some(sel) if contrast(text, sel) >= MIN_ROLE_CONTRAST => sel,
        _ => lerp(bg, text, 0.16),
    };
    let fix = |c: Rgb| ensure_contrast(c, bg, text, MIN_ROLE_CONTRAST);
    Theme {
        support: ColorSupport::TrueColor,
        no_color: false,
        bg,
        surface: lerp(bg, text, 0.04),
        raised: lerp(bg, text, 0.07),
        highlight,
        border: lerp(bg, text, 0.22),
        text,
        muted: fix(lerp(bg, text, 0.55)),
        accent: fix(p[6]),
        accent_alt: fix(p[5]),
        user: fix(p[4]),
        assistant: fix(p[5]),
        success: fix(p[2]),
        warning: fix(p[3]),
        error: fix(p[1]),
        diff_add_bg: lerp(bg, p[2], 0.18),
        diff_del_bg: lerp(bg, p[1], 0.18),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRUVBOX_LIGHT_WARNING: Rgb = Rgb(0xd7, 0x99, 0x21);
    const GRUVBOX_LIGHT_BG: Rgb = Rgb(0xfb, 0xf1, 0xc7);

    fn sample_base() -> BasePalette {
        let mut palette = [Rgb(0, 0, 0); 16];
        for (i, slot) in palette.iter_mut().enumerate() {
            *slot = Rgb(i as u8 * 10, 0x20, 0xf0);
        }
        palette[1] = Rgb(0xfb, 0x49, 0x34);
        palette[2] = Rgb(0xb8, 0xbb, 0x26);
        palette[3] = Rgb(0xfa, 0xbd, 0x2f);
        palette[4] = Rgb(0x83, 0xa5, 0x98);
        palette[5] = Rgb(0xd3, 0x86, 0x9b);
        palette[6] = Rgb(0x8e, 0xc0, 0x7c);
        BasePalette {
            background: Rgb(0x28, 0x28, 0x28),
            foreground: Rgb(0xeb, 0xdb, 0xb2),
            palette,
            selection_background: None,
        }
    }

    #[test]
    fn parse_hex_accepts_long_short_and_bare_forms() {
        assert_eq!(parse_hex("#1d2021"), Some(Rgb(0x1d, 0x20, 0x21)));
        assert_eq!(parse_hex("1D2021"), Some(Rgb(0x1d, 0x20, 0x21)));
        assert_eq!(parse_hex("#fff"), Some(Rgb(255, 255, 255)));
    }

    #[test]
    fn parse_hex_rejects_bad_input() {
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("zzzzzz"), None);
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#"), None);
    }

    #[test]
    fn parse_ghostty_reads_a_full_file_and_ignores_extra_keys() {
        let text = "# comment\n\nbackground = #1d2021\nforeground = #ebdbb2\n\
                    cursor-color = #ebdbb2\ncursor-text = #1d2021\n\
                    selection-background = #665c54\nselection-foreground = #ebdbb2\n\
                    palette = 0=#1d2021\npalette = 1=#fb4934\npalette = 2=#b8bb26\n\
                    palette = 3=#fabd2f\npalette = 4=#83a598\npalette = 5=#d3869b\n\
                    palette = 6=#8ec07c\npalette = 7=#ebdbb2\npalette = 8=#928374\n\
                    palette = 9=#fb4934\npalette = 10=#b8bb26\npalette = 11=#fabd2f\n\
                    palette = 12=#83a598\npalette = 13=#d3869b\npalette = 14=#8ec07c\n\
                    palette = 15=#ebdbb2\n";
        let base = parse_ghostty(text).expect("valid file");
        assert_eq!(base.background, Rgb(0x1d, 0x20, 0x21));
        assert_eq!(base.foreground, Rgb(0xeb, 0xdb, 0xb2));
        assert_eq!(base.selection_background, Some(Rgb(0x66, 0x5c, 0x54)));
        assert_eq!(base.palette[1], Rgb(0xfb, 0x49, 0x34));
        assert_eq!(base.palette[15], Rgb(0xeb, 0xdb, 0xb2));
    }

    #[test]
    fn parse_ghostty_names_a_missing_palette_entry() {
        let mut text = String::from("background = #000000\nforeground = #ffffff\n");
        for i in 0..16 {
            if i != 7 {
                text.push_str(&format!("palette = {i}=#101010\n"));
            }
        }
        let err = parse_ghostty(&text).unwrap_err();
        assert!(err.contains("palette = 7"), "{err}");
    }

    #[test]
    fn parse_ghostty_reports_the_line_of_a_bad_colour() {
        let text = "background = #000000\nforeground = #zzzzzz\n";
        let err = parse_ghostty(text).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn contrast_of_black_and_white_is_21() {
        let ratio = contrast(Rgb(0, 0, 0), Rgb(255, 255, 255));
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn contrast_of_identical_colours_is_1() {
        let ratio = contrast(Rgb(0x12, 0x34, 0x56), Rgb(0x12, 0x34, 0x56));
        assert!((ratio - 1.0).abs() < 1e-6, "{ratio}");
    }

    #[test]
    fn luminance_orders_black_below_white() {
        assert!(luminance(Rgb(0, 0, 0)) < 0.001);
        assert!((luminance(Rgb(255, 255, 255)) - 1.0).abs() < 0.001);
    }

    #[test]
    fn ensure_contrast_lifts_gruvbox_light_warning() {
        let fixed = ensure_contrast(
            GRUVBOX_LIGHT_WARNING,
            GRUVBOX_LIGHT_BG,
            Rgb(0x3c, 0x38, 0x36),
            MIN_ROLE_CONTRAST,
        );
        assert!(contrast(fixed, GRUVBOX_LIGHT_BG) >= 3.0);
        assert_ne!(fixed, GRUVBOX_LIGHT_WARNING);
    }

    #[test]
    fn ensure_contrast_leaves_a_passing_colour_unchanged() {
        let white = Rgb(255, 255, 255);
        let black = Rgb(0, 0, 0);
        assert_eq!(
            ensure_contrast(white, black, Rgb(128, 128, 128), 3.0),
            white
        );
    }

    #[test]
    fn derive_maps_base_colours_to_roles() {
        let base = sample_base();
        let theme = derive(&base);
        assert_eq!(theme.bg, base.background);
        assert_eq!(theme.text, base.foreground);
        assert_eq!(theme.support, ColorSupport::TrueColor);
        assert!(!theme.no_color);
        assert_eq!(theme.error, base.palette[1]);
        assert_eq!(theme.success, base.palette[2]);
        assert_eq!(theme.accent, base.palette[6]);
        assert_eq!(
            theme.highlight,
            lerp(base.background, base.foreground, 0.16)
        );
    }

    #[test]
    fn derive_uses_selection_background_when_readable() {
        let mut base = sample_base();
        base.selection_background = Some(Rgb(0x66, 0x5c, 0x54));
        assert_eq!(derive(&base).highlight, Rgb(0x66, 0x5c, 0x54));
    }

    #[test]
    fn derive_ignores_unreadable_selection_background() {
        let mut base = sample_base();
        base.selection_background = Some(Rgb(0xe8, 0xd8, 0xa8));
        assert_eq!(
            derive(&base).highlight,
            lerp(base.background, base.foreground, 0.16)
        );
    }
}
