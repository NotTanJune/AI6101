//! Theme selection from `--theme`, `MCPE_THEME` and `config.json` files.
//!
//! [`load`] is pure: the caller reads the config files and supplies a reader for `theme-file`.
//! Explicit inputs (`--theme`, `MCPE_THEME`) fail fast with an error. Problems inside config
//! files never fail; each becomes one warning string and the bad item is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::builtin::{self, DEFAULT_SLUG};
use super::palette::{BasePalette, derive, parse_ghostty, parse_hex};
use super::{Rgb, Theme};

/// Semantic colour keys accepted under `colors` (kebab case of the [`Theme`] fields).
const SEMANTIC_KEYS: &[&str] = &[
    "bg",
    "surface",
    "raised",
    "highlight",
    "border",
    "text",
    "muted",
    "accent",
    "accent-alt",
    "user",
    "assistant",
    "success",
    "warning",
    "error",
    "diff-add-bg",
    "diff-del-bg",
];

/// Ghostty keys accepted and ignored under `colors`, so a pasted Ghostty theme block is quiet.
const IGNORED_COLOR_KEYS: &[&str] = &["cursor-color", "cursor-text", "selection-foreground"];

/// A config file that has already been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub text: String,
}

/// The explicit theme inputs: the `--theme` flag and the `MCPE_THEME` variable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeChoice {
    pub flag: Option<String>,
    pub env: Option<String>,
}

/// Where the base palette comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeBase {
    /// A built-in theme, by slug.
    Builtin(&'static str),
    /// A Ghostty-format file chosen with `theme-file`.
    Custom { label: String, palette: BasePalette },
}

/// Base palette keys set under `colors`, merged across config files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BaseOverrides {
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
    pub selection_background: Option<Rgb>,
    pub palette: [Option<Rgb>; 16],
}

/// A resolved, reusable theme selection: base, base overrides and semantic overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSpec {
    pub base: ThemeBase,
    pub base_overrides: BaseOverrides,
    /// Semantic overrides keyed by kebab-case role name. Applied exactly, without contrast
    /// adjustment.
    pub semantic: BTreeMap<String, Rgb>,
}

impl Default for ThemeSpec {
    /// The built-in `tokyo-night` with no overrides.
    fn default() -> Self {
        Self {
            base: ThemeBase::Builtin(DEFAULT_SLUG),
            base_overrides: BaseOverrides::default(),
            semantic: BTreeMap::new(),
        }
    }
}

/// Check that `text` is a JSON document. Used for an explicit `--config` file, which fails
/// before the terminal opens instead of becoming a warning.
pub fn check_json(text: &str) -> Result<(), String> {
    serde_json::from_str::<Value>(text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Resolve the effective theme selection.
///
/// Precedence for the base: `choice.flag`, then `choice.env`, then the effective config
/// (`theme-file` wins over `theme`), then `tokyo-night`. Colour overrides from every config
/// file always apply. The returned warnings describe bad config content.
///
/// Returns `Err` only when the flag or environment value names an unknown theme.
pub fn load(
    sources: &[ConfigSource],
    choice: &ThemeChoice,
    read: &dyn Fn(&Path) -> Result<String, String>,
    home: Option<&Path>,
) -> Result<(ThemeSpec, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut base_overrides = BaseOverrides::default();
    let mut semantic = BTreeMap::new();
    let mut config_theme: Option<&'static str> = None;
    let mut config_theme_file: Option<PathBuf> = None;

    for source in sources {
        let file = source.path.display().to_string();
        let value: Value = match serde_json::from_str(&source.text) {
            Ok(value) => value,
            Err(err) => {
                warnings.push(format!("{file}: invalid JSON ({err}); file ignored"));
                continue;
            }
        };
        let Some(object) = value.as_object() else {
            warnings.push(format!(
                "{file}: top level must be a JSON object; file ignored"
            ));
            continue;
        };
        let dir = source.path.parent().unwrap_or_else(|| Path::new("."));
        for (key, val) in object {
            match key.as_str() {
                "theme" => match val.as_str() {
                    Some(name) => match builtin::find(name) {
                        Some(b) => config_theme = Some(b.slug),
                        None => warnings.push(format!(
                            "{file}: unknown theme {name:?}; valid themes: {}",
                            builtin::names().join(", ")
                        )),
                    },
                    None => warnings.push(format!("{file}: \"theme\" must be a string")),
                },
                "theme-file" => match val.as_str() {
                    Some(raw) => config_theme_file = Some(resolve_path(raw, dir, home)),
                    None => warnings.push(format!("{file}: \"theme-file\" must be a string")),
                },
                "colors" => parse_colors(
                    val,
                    &file,
                    &mut base_overrides,
                    &mut semantic,
                    &mut warnings,
                ),
                other => warnings.push(format!("{file}: unknown key {other:?}")),
            }
        }
    }

    let explicit = match (choice.flag.as_deref(), choice.env.as_deref()) {
        (Some(name), _) => Some(("--theme", name)),
        (None, Some(name)) if !name.trim().is_empty() => Some(("MCPE_THEME", name)),
        _ => None,
    };

    let mut base = None;
    if let Some((origin, name)) = explicit {
        let b = builtin::find(name).ok_or_else(|| {
            format!(
                "unknown theme {name:?} from {origin}; valid themes: {}",
                builtin::names().join(", ")
            )
        })?;
        base = Some(ThemeBase::Builtin(b.slug));
    } else if let Some(path) = &config_theme_file {
        if config_theme.is_some() {
            warnings.push("\"theme\" ignored because \"theme-file\" is also set".to_owned());
        }
        match load_custom(path, read) {
            Ok(custom) => base = Some(custom),
            Err(warning) => warnings.push(warning),
        }
    }
    let base = base.unwrap_or(ThemeBase::Builtin(config_theme.unwrap_or(DEFAULT_SLUG)));

    Ok((
        ThemeSpec {
            base,
            base_overrides,
            semantic,
        },
        warnings,
    ))
}

fn load_custom(
    path: &Path,
    read: &dyn Fn(&Path) -> Result<String, String>,
) -> Result<ThemeBase, String> {
    let text =
        read(path).map_err(|err| format!("{}: cannot read theme-file: {err}", path.display()))?;
    let palette = parse_ghostty(&text)
        .map_err(|err| format!("{}: invalid theme-file: {err}", path.display()))?;
    Ok(ThemeBase::Custom {
        label: path.display().to_string(),
        palette,
    })
}

/// `~/` expands to `home` when known; other relative paths resolve against `dir`.
fn resolve_path(raw: &str, dir: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (raw.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    }
}

fn parse_colors(
    value: &Value,
    file: &str,
    base: &mut BaseOverrides,
    semantic: &mut BTreeMap<String, Rgb>,
    warnings: &mut Vec<String>,
) {
    let Some(object) = value.as_object() else {
        warnings.push(format!("{file}: \"colors\" must be an object"));
        return;
    };
    for (key, val) in object {
        let key = key.as_str();
        match key {
            "background" | "foreground" | "selection-background" => {
                match val.as_str().and_then(parse_hex) {
                    Some(colour) => match key {
                        "background" => base.background = Some(colour),
                        "foreground" => base.foreground = Some(colour),
                        _ => base.selection_background = Some(colour),
                    },
                    None => warnings.push(format!("{file}: colors.{key} is not a valid colour")),
                }
            }
            "palette" => match val.as_array() {
                Some(items) => {
                    for item in items {
                        match item.as_str().map(parse_palette_entry) {
                            Some(Ok((slot, colour))) => base.palette[slot] = Some(colour),
                            Some(Err(err)) => {
                                warnings.push(format!("{file}: colors.palette: {err}"));
                            }
                            None => warnings.push(format!(
                                "{file}: colors.palette entries must be \"N=#hex\" strings"
                            )),
                        }
                    }
                }
                None => warnings.push(format!(
                    "{file}: colors.palette must be an array of \"N=#hex\" strings"
                )),
            },
            _ if IGNORED_COLOR_KEYS.contains(&key) => {}
            _ if SEMANTIC_KEYS.contains(&key) => match val.as_str().and_then(parse_hex) {
                Some(colour) => {
                    semantic.insert(key.to_owned(), colour);
                }
                None => warnings.push(format!("{file}: colors.{key} is not a valid colour")),
            },
            other => warnings.push(format!("{file}: unknown key colors.{other}")),
        }
    }
}

/// Parse one `N=#hex` palette entry, with `N` in `0..=15`.
fn parse_palette_entry(text: &str) -> Result<(usize, Rgb), String> {
    let (index, hex) = text
        .split_once('=')
        .ok_or_else(|| format!("entry {text:?} must be N=#hex"))?;
    let slot = index
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n < 16)
        .ok_or_else(|| format!("palette index in {text:?} must be 0..=15"))?;
    let colour = parse_hex(hex.trim()).ok_or_else(|| format!("invalid colour in {text:?}"))?;
    Ok((slot, colour))
}

/// Set one semantic role by its kebab-case key. Returns `false` for an unknown key.
pub(crate) fn set_role(theme: &mut Theme, key: &str, colour: Rgb) -> bool {
    let slot = match key {
        "bg" => &mut theme.bg,
        "surface" => &mut theme.surface,
        "raised" => &mut theme.raised,
        "highlight" => &mut theme.highlight,
        "border" => &mut theme.border,
        "text" => &mut theme.text,
        "muted" => &mut theme.muted,
        "accent" => &mut theme.accent,
        "accent-alt" => &mut theme.accent_alt,
        "user" => &mut theme.user,
        "assistant" => &mut theme.assistant,
        "success" => &mut theme.success,
        "warning" => &mut theme.warning,
        "error" => &mut theme.error,
        "diff-add-bg" => &mut theme.diff_add_bg,
        "diff-del-bg" => &mut theme.diff_del_bg,
        _ => return false,
    };
    *slot = colour;
    true
}

fn apply_base_overrides(base: &mut BasePalette, overrides: &BaseOverrides) {
    if let Some(colour) = overrides.background {
        base.background = colour;
    }
    if let Some(colour) = overrides.foreground {
        base.foreground = colour;
    }
    if let Some(colour) = overrides.selection_background {
        base.selection_background = Some(colour);
    }
    for (slot, entry) in overrides.palette.iter().enumerate() {
        if let Some(colour) = entry {
            base.palette[slot] = *colour;
        }
    }
}

impl ThemeSpec {
    /// Whether the base is a `theme-file` rather than a built-in.
    pub fn is_custom(&self) -> bool {
        matches!(self.base, ThemeBase::Custom { .. })
    }

    /// Name shown in the picker and status output: the slug, or the custom file path.
    pub fn label(&self) -> String {
        match &self.base {
            ThemeBase::Builtin(slug) => (*slug).to_owned(),
            ThemeBase::Custom { label, .. } => label.clone(),
        }
    }

    /// The same colour overrides on a different built-in base. `None` for an unknown name.
    pub fn with_builtin(&self, slug: &str) -> Option<ThemeSpec> {
        let found = builtin::find(slug)?;
        Some(ThemeSpec {
            base: ThemeBase::Builtin(found.slug),
            base_overrides: self.base_overrides.clone(),
            semantic: self.semantic.clone(),
        })
    }

    /// The theme to render: base, then base overrides, then derivation (or the hand-tuned
    /// Tokyo Night when there are no base overrides), then semantic overrides.
    ///
    /// The result is true colour with `no_color: false`; callers apply
    /// [`Theme::with_terminal`].
    pub fn resolve(&self) -> Theme {
        let no_base_overrides = self.base_overrides == BaseOverrides::default();
        let mut theme = match &self.base {
            ThemeBase::Builtin(slug) => match builtin::find(slug) {
                Some(_) if *slug == DEFAULT_SLUG && no_base_overrides => Theme::default(),
                Some(found) => {
                    let mut base = builtin::base(found);
                    apply_base_overrides(&mut base, &self.base_overrides);
                    derive(&base)
                }
                None => Theme::default(),
            },
            ThemeBase::Custom { palette, .. } => {
                let mut base = palette.clone();
                apply_base_overrides(&mut base, &self.base_overrides);
                derive(&base)
            }
        };
        for (key, colour) in &self.semantic {
            set_role(&mut theme, key, *colour);
        }
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str, text: &str) -> ConfigSource {
        ConfigSource {
            path: PathBuf::from(path),
            text: text.to_owned(),
        }
    }

    fn no_read(_: &Path) -> Result<String, String> {
        Err("no file access expected".to_owned())
    }

    fn load_plain(sources: &[ConfigSource]) -> (ThemeSpec, Vec<String>) {
        load(sources, &ThemeChoice::default(), &no_read, None).expect("config loads")
    }

    fn nord_text() -> String {
        builtin::find("nord").expect("nord").file_text.to_owned()
    }

    #[test]
    fn global_theme_is_used_and_workspace_overrides_it() {
        let global = source(
            "/home/u/.config/mcpe/config.json",
            r#"{"theme": "gruvbox-dark"}"#,
        );
        let (spec, warnings) = load_plain(std::slice::from_ref(&global));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(spec.base, ThemeBase::Builtin("gruvbox-dark"));

        let workspace = source("/w/.mcpe/config.json", r#"{"theme": "nord"}"#);
        let (spec, _) = load_plain(&[global, workspace]);
        assert_eq!(spec.base, ThemeBase::Builtin("nord"));
    }

    #[test]
    fn workspace_colour_overrides_global_colour() {
        let global = source("/g/config.json", r##"{"colors": {"accent": "#111111"}}"##);
        let workspace = source("/w/config.json", r##"{"colors": {"accent": "#222222"}}"##);
        let (spec, _) = load_plain(&[global, workspace]);
        assert_eq!(spec.semantic.get("accent"), Some(&Rgb(0x22, 0x22, 0x22)));
    }

    #[test]
    fn palette_entries_merge_by_index() {
        let global = source(
            "/g/config.json",
            r##"{"colors": {"palette": ["1=#fb4934", "4=#83a598"]}}"##,
        );
        let workspace = source(
            "/w/config.json",
            r##"{"colors": {"palette": ["1=#ff0000"]}}"##,
        );
        let (spec, _) = load_plain(&[global, workspace]);
        assert_eq!(spec.base_overrides.palette[1], Some(Rgb(0xff, 0, 0)));
        assert_eq!(spec.base_overrides.palette[4], Some(Rgb(0x83, 0xa5, 0x98)));
    }

    #[test]
    fn base_background_override_changes_derived_surface() {
        let config = source(
            "/g/config.json",
            r##"{"colors": {"background": "#000000"}}"##,
        );
        let (spec, _) = load_plain(&[config]);
        let theme = spec.resolve();
        assert_eq!(theme.bg, Rgb(0, 0, 0));
        assert_ne!(theme.surface, Theme::default().surface);
    }

    #[test]
    fn semantic_override_is_exact_without_contrast_adjustment() {
        let config = source("/g/config.json", r##"{"colors": {"accent": "#1a1b26"}}"##);
        let (spec, _) = load_plain(&[config]);
        assert_eq!(spec.resolve().accent, Rgb(0x1a, 0x1b, 0x26));
    }

    #[test]
    fn short_hex_is_accepted() {
        let config = source("/g/config.json", r##"{"colors": {"accent": "#f00"}}"##);
        let (spec, warnings) = load_plain(&[config]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(spec.semantic.get("accent"), Some(&Rgb(255, 0, 0)));
    }

    #[test]
    fn ignored_ghostty_keys_do_not_warn() {
        let config = source(
            "/g/config.json",
            r##"{"colors": {"cursor-color": "#ffffff", "cursor-text": "#000000", "selection-foreground": "#ffffff"}}"##,
        );
        let (_, warnings) = load_plain(&[config]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn bad_items_warn_once_each_and_the_rest_still_applies() {
        let text = r##"{
            "bogus": 1,
            "theme": "nope",
            "colors": {
                "accent": "#zzz",
                "palette": ["16=#ffffff"],
                "nonsense": "#fff",
                "accent-alt": "#abcdef"
            }
        }"##;
        let (spec, warnings) = load_plain(&[source("/g/config.json", text)]);
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("/g/config.json")));
        assert_eq!(
            spec.semantic.get("accent-alt"),
            Some(&Rgb(0xab, 0xcd, 0xef))
        );
        assert!(!spec.semantic.contains_key("accent"));
    }

    #[test]
    fn invalid_json_warns_and_is_ignored() {
        let (spec, warnings) = load_plain(&[source("/g/config.json", "{ nope")]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(spec, ThemeSpec::default());
    }

    #[test]
    fn theme_file_resolves_relative_and_tilde_paths() {
        let text = nord_text();
        let read = move |path: &Path| -> Result<String, String> {
            if path.ends_with("themes/x.ghostty") || path.ends_with("t.ghostty") {
                Ok(text.clone())
            } else {
                Err(format!("unexpected path {}", path.display()))
            }
        };
        let relative = source(
            "/cfg/dir/config.json",
            r#"{"theme-file": "themes/x.ghostty"}"#,
        );
        let (spec, warnings) = load(&[relative], &ThemeChoice::default(), &read, None).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(spec.label(), "/cfg/dir/themes/x.ghostty");

        let tilde = source("/cfg/dir/config.json", r#"{"theme-file": "~/t.ghostty"}"#);
        let home = Path::new("/home/u");
        let (spec, _) = load(&[tilde], &ThemeChoice::default(), &read, Some(home)).unwrap();
        assert_eq!(spec.label(), "/home/u/t.ghostty");
    }

    #[test]
    fn theme_file_wins_over_theme_with_a_warning() {
        let text = nord_text();
        let read = move |_: &Path| -> Result<String, String> { Ok(text.clone()) };
        let global = source("/g/config.json", r#"{"theme": "nord"}"#);
        let workspace = source("/w/config.json", r#"{"theme-file": "/abs/t.ghostty"}"#);
        let (spec, warnings) =
            load(&[global, workspace], &ThemeChoice::default(), &read, None).unwrap();
        assert!(spec.is_custom());
        assert!(
            warnings.iter().any(|w| w.contains("\"theme\" ignored")),
            "{warnings:?}"
        );
    }

    #[test]
    fn flag_beats_env_beats_config() {
        let config = source("/g/config.json", r#"{"theme": "gruvbox-dark"}"#);
        let both = ThemeChoice {
            flag: Some("dracula".to_owned()),
            env: Some("nord".to_owned()),
        };
        let (spec, _) = load(std::slice::from_ref(&config), &both, &no_read, None).unwrap();
        assert_eq!(spec.base, ThemeBase::Builtin("dracula"));

        let env_only = ThemeChoice {
            flag: None,
            env: Some("Nord".to_owned()),
        };
        let (spec, _) = load(std::slice::from_ref(&config), &env_only, &no_read, None).unwrap();
        assert_eq!(spec.base, ThemeBase::Builtin("nord"));

        let (spec, _) = load(&[config], &ThemeChoice::default(), &no_read, None).unwrap();
        assert_eq!(spec.base, ThemeBase::Builtin("gruvbox-dark"));
    }

    #[test]
    fn unknown_flag_is_an_error_listing_the_slugs() {
        let choice = ThemeChoice {
            flag: Some("nope".to_owned()),
            env: None,
        };
        let err = load(&[], &choice, &no_read, None).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("gruvbox-dark-hard"), "{err}");
    }

    #[test]
    fn colour_overrides_survive_a_theme_switch() {
        let config = source("/g/config.json", r##"{"colors": {"accent": "#ff5555"}}"##);
        let (spec, _) = load_plain(&[config]);
        let switched = spec.with_builtin("nord").expect("nord exists");
        assert_eq!(switched.base, ThemeBase::Builtin("nord"));
        assert_eq!(switched.resolve().accent, Rgb(0xff, 0x55, 0x55));
        assert!(spec.with_builtin("nope").is_none());
    }

    #[test]
    fn every_semantic_key_maps_to_a_theme_role() {
        let mut theme = Theme::default();
        for key in SEMANTIC_KEYS {
            assert!(set_role(&mut theme, key, Rgb(1, 2, 3)), "{key}");
        }
        assert!(!set_role(&mut theme, "not-a-role", Rgb(1, 2, 3)));
    }
}
