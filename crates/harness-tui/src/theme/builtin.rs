//! The built-in colour themes, embedded from `crates/harness-tui/themes/*.ghostty`.

use super::Theme;
use super::palette::{BasePalette, derive, parse_ghostty};

/// Slug of the hand-tuned default theme.
pub const DEFAULT_SLUG: &str = "tokyo-night";

/// Where Ghostty keeps its bundled themes on macOS. Used only by the drift-guard test.
#[cfg(test)]
const GHOSTTY_THEME_DIR: &str = "/Applications/Ghostty.app/Contents/Resources/ghostty/themes";

/// One built-in theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    /// Canonical name, shown by `/theme` and in `--help`.
    pub slug: &'static str,
    /// Other accepted names (already normalised).
    pub extra_names: &'static [&'static str],
    /// Ghostty file name the embedded copy was taken from.
    pub source: &'static str,
    /// Embedded Ghostty-format theme text.
    pub file_text: &'static str,
}

macro_rules! builtin {
    ($slug:literal, [$($extra:literal),* $(,)?], $source:literal) => {
        Builtin {
            slug: $slug,
            extra_names: &[$($extra),*],
            source: $source,
            file_text: include_str!(concat!("../../themes/", $slug, ".ghostty")),
        }
    };
}

/// Every built-in, in display order. `tokyo-night` comes first.
pub const BUILTINS: &[Builtin] = &[
    builtin!(
        "tokyo-night",
        ["tokyonight", "tokyonight-night"],
        "TokyoNight Night"
    ),
    builtin!("gruvbox-dark", [], "Gruvbox Dark"),
    builtin!("gruvbox-dark-hard", [], "Gruvbox Dark Hard"),
    builtin!("gruvbox-light", [], "Gruvbox Light"),
    builtin!("catppuccin-mocha", [], "Catppuccin Mocha"),
    builtin!("catppuccin-macchiato", [], "Catppuccin Macchiato"),
    builtin!("catppuccin-frappe", [], "Catppuccin Frappe"),
    builtin!("catppuccin-latte", [], "Catppuccin Latte"),
    builtin!("nord", [], "Nord"),
    builtin!("monokai", ["monokai-classic"], "Monokai Classic"),
    builtin!("monokai-pro", [], "Monokai Pro"),
    builtin!("dracula", [], "Dracula"),
    builtin!("one-dark", ["atom-one-dark"], "Atom One Dark"),
    builtin!(
        "solarized-dark",
        ["iterm2-solarized-dark"],
        "iTerm2 Solarized Dark"
    ),
    builtin!(
        "solarized-light",
        ["iterm2-solarized-light"],
        "iTerm2 Solarized Light"
    ),
    builtin!("rose-pine", [], "Rose Pine"),
    builtin!("rose-pine-moon", [], "Rose Pine Moon"),
    builtin!("rose-pine-dawn", [], "Rose Pine Dawn"),
    builtin!(
        "everforest-dark",
        ["everforest-dark-hard"],
        "Everforest Dark Hard"
    ),
    builtin!("kanagawa-wave", [], "Kanagawa Wave"),
];

/// Every slug, in table order.
pub fn names() -> Vec<&'static str> {
    BUILTINS.iter().map(|b| b.slug).collect()
}

/// Normalise a user supplied theme name: trim, lowercase, and turn each run of spaces,
/// underscores or hyphens into a single `-`.
pub fn normalise(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars() {
        if ch == ' ' || ch == '_' || ch == '-' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Find a built-in by slug, then by extra accepted name.
pub fn find(name: &str) -> Option<&'static Builtin> {
    let wanted = normalise(name);
    BUILTINS.iter().find(|b| b.slug == wanted).or_else(|| {
        BUILTINS
            .iter()
            .find(|b| b.extra_names.contains(&wanted.as_str()))
    })
}

/// The base palette of a built-in. The embedded files are checked by tests, so a parse
/// failure here is a programming error.
pub fn base(builtin: &Builtin) -> BasePalette {
    parse_ghostty(builtin.file_text)
        .unwrap_or_else(|err| panic!("embedded theme {} is invalid: {err}", builtin.slug))
}

/// The resolved theme of a built-in with no overrides. `tokyo-night` is exactly
/// [`Theme::default`]; every other built-in is derived from its base palette.
pub fn theme(builtin: &Builtin) -> Theme {
    if builtin.slug == DEFAULT_SLUG {
        Theme::default()
    } else {
        derive(&base(builtin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Rgb;
    use crate::theme::palette::{MIN_ROLE_CONTRAST, contrast};

    #[test]
    fn names_lists_every_slug_with_tokyo_night_first() {
        let names = names();
        assert_eq!(names.len(), 20);
        assert_eq!(names[0], "tokyo-night");
        assert!(names.contains(&"gruvbox-dark-hard"));
    }

    #[test]
    fn every_embedded_file_parses() {
        for b in BUILTINS {
            let _ = base(b);
        }
    }

    #[test]
    fn every_builtin_has_readable_roles() {
        let mut failures = Vec::new();
        for b in BUILTINS {
            let t = theme(b);
            let text_ratio = contrast(t.text, t.bg);
            if text_ratio < 4.5 {
                failures.push(format!("{}: text on bg is {text_ratio:.2}", b.slug));
            }
            for (role, colour) in [
                ("muted", t.muted),
                ("accent", t.accent),
                ("accent_alt", t.accent_alt),
                ("user", t.user),
                ("assistant", t.assistant),
                ("success", t.success),
                ("warning", t.warning),
                ("error", t.error),
            ] {
                let ratio = contrast(colour, t.bg);
                if ratio < MIN_ROLE_CONTRAST {
                    failures.push(format!("{}: {role} on bg is {ratio:.2}", b.slug));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "contrast failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn solarized_light_text_is_darkened_to_readable_contrast() {
        let b = find("solarized-light").expect("solarized-light exists");
        let t = theme(b);
        assert_ne!(t.text, Rgb(0x65, 0x7b, 0x83));
        assert!(contrast(t.text, t.bg) >= 4.5);
    }

    #[test]
    fn passing_foregrounds_are_kept_exactly() {
        for b in BUILTINS {
            let base = base(b);
            if contrast(base.foreground, base.background) >= 4.5 {
                assert_eq!(theme(b).text, base.foreground, "{}", b.slug);
            }
        }
    }

    #[test]
    fn tokyo_night_is_the_hand_tuned_default() {
        let b = find("tokyo-night").expect("tokyo-night exists");
        assert_eq!(theme(b), Theme::default());
    }

    #[test]
    fn embedded_files_match_the_ghostty_originals_when_installed() {
        for b in BUILTINS {
            let path = std::path::Path::new(GHOSTTY_THEME_DIR).join(b.source);
            if !path.exists() {
                eprintln!(
                    "skipping drift guard for {}: {} not found",
                    b.slug,
                    path.display()
                );
                continue;
            }
            let original = std::fs::read(&path).expect("readable Ghostty theme");
            assert_eq!(
                original,
                b.file_text.as_bytes(),
                "{} differs from {}",
                b.slug,
                path.display()
            );
        }
    }

    #[test]
    fn name_matching_accepts_spacing_case_and_separator_variants() {
        for name in [
            "Gruvbox Dark Hard",
            "gruvbox_dark_hard",
            "  GRUVBOX-dark-hard ",
        ] {
            assert_eq!(
                find(name).map(|b| b.slug),
                Some("gruvbox-dark-hard"),
                "{name}"
            );
        }
    }

    #[test]
    fn extra_names_resolve_to_their_slug() {
        assert_eq!(find("Monokai Classic").map(|b| b.slug), Some("monokai"));
        assert_eq!(find("Atom One Dark").map(|b| b.slug), Some("one-dark"));
        assert_eq!(find("tokyonight").map(|b| b.slug), Some("tokyo-night"));
        assert!(find("nope").is_none());
    }

    #[test]
    fn normalise_collapses_separator_runs() {
        assert_eq!(normalise("  Rose   Pine_Moon "), "rose-pine-moon");
    }
}
