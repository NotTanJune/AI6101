# Plan 005: Add built-in colour themes and a `config.json` for colour overrides to the `mcpe` TUI

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update the status row for this plan in
> `plans/README.md`, unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: the repository has no commits (unborn `main`, every file
> untracked), so there is no SHA to diff against. Instead compare the "Current state" excerpts
> below with the live files (`crates/harness-tui/src/theme.rs`, `crates/harness-tui/src/terminal.rs`,
> `crates/harness-tui/src/app.rs`, `crates/harness-cli/src/launcher.rs`,
> `crates/harness-cli/src/bin/mcpe.rs`). On a mismatch, STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW (visual only; no agent, cost or approval logic changes)
- **Depends on**: plans/003-coding-agent-tui.md (DONE) and the follow-up TUI polish (theme module, `mcpe` launcher), both already in the working tree
- **Category**: direction
- **Planned at**: no commit SHA (unborn `main`), 2026-10-10

## Why this matters

The `mcpe` TUI has exactly one hard-coded colour scheme (a Tokyo Night style dark palette in
`Theme::default()`). The owner wants the common terminal themes (Gruvbox, Catppuccin, Nord,
Monokai and similar) and a `config.json` where colours can be written as `key: "#hex"` pairs,
the same way Ghostty's config works (the owner's Ghostty config sets `theme = Gruvbox Dark Hard`
and lines like `split-divider-color = #ff5555`). After this plan: `mcpe --theme nord`, a
`theme` key in `~/.config/mcpe/config.json` or `<workspace>/.mcpe/config.json`, per-colour
overrides in the same file, and a `/theme` command with live preview all work. Nothing about
the agent, approvals or cost changes.

## Current state

### Theme module (`crates/harness-tui/src/theme.rs`, 416 lines)

- `Rgb(pub u8, pub u8, pub u8)`, `ColorSupport { TrueColor, Indexed256, Basic }` with
  `ColorSupport::detect(env)`, free functions `lerp`, `dim`, `color_to_rgb`, and the
  private `nearest_basic`, `nearest_indexed` mappers.
- `Theme` has two terminal fields and sixteen semantic colour roles:

```rust
// crates/harness-tui/src/theme.rs:156-193 (abridged)
pub struct Theme {
    pub support: ColorSupport,
    pub no_color: bool,
    pub bg: Rgb, pub surface: Rgb, pub raised: Rgb, pub highlight: Rgb,
    pub border: Rgb, pub text: Rgb, pub muted: Rgb,
    pub accent: Rgb, pub accent_alt: Rgb, pub user: Rgb, pub assistant: Rgb,
    pub success: Rgb, pub warning: Rgb, pub error: Rgb,
    pub diff_add_bg: Rgb, pub diff_del_bg: Rgb,
}
```

- `impl Default for Theme` (lines ~195-218) holds the current hand-tuned values:
  bg `#1a1b26`, surface `#1f2335`, raised `#24283b`, highlight `#333a57`, border `#3b4261`,
  text `#c0caf5`, muted `#6c75a0`, accent `#7dcfff`, accent_alt `#bb9af7`, user `#7aa2f7`,
  assistant `#bb9af7`, success `#9ece6a`, warning `#e0af68`, error `#f7768e`,
  diff_add_bg `#20303b`, diff_del_bg `#37222c`. **Many render tests assert these exact
  values; they must not change.**
- `Theme::detect(env)` returns `Self { support, no_color, ..Self::default() }`;
  `Theme::from_process_env()` calls it with `std::env::var`.
- Tests live in a `#[cfg(test)] mod tests` at the bottom, using an `env_of(&[(k, v)])`
  closure helper. Match that style for new pure-function tests.

### Where the theme is set

- `crates/harness-tui/src/app.rs:313-315` — `App` has `pub theme: Theme`, initialised to
  `Theme::default()` at line ~389.
- `crates/harness-tui/src/terminal.rs:250`, inside `pub fn run(mut app: App, ...)`:

```rust
    app.theme = crate::theme::Theme::from_process_env();
```

  This **overwrites the whole theme** with the default palette. Any theme chosen before
  `run` would be lost. It must only change `support` and `no_color`.

### Picker and commands

- `crates/harness-tui/src/app.rs:187-208` — `PickerItem { label, detail, value }` and
  `PickerView { title, items, selected }`. There is only one picker today (models).
- `crates/harness-tui/src/app.rs:696-718` — `on_picker_key`: Up/Down/j/k move, Esc/q close,
  Enter calls `self.switch_model(&alias)` unconditionally. A theme picker needs a picker kind.
- `crates/harness-tui/src/commands.rs:29-48` — `pub enum SlashCommand { Model(Option<String>),
  Effort(String), Mcp(..), Kb(..), Drift, Cost, Clear, Help, Quit }`, with `USAGE_*` consts
  and `pub const COMMAND_HELP: &[(&str, &str)]` at line 98. `COMMAND_HELP` is the single
  source for help rows and for autocomplete command names (`completion.rs::command_table`).
- `crates/harness-tui/src/completion.rs:127` — `pub fn suggest(input, ctx)`; argument
  completion is a `match` on `(command, arg_index)`, e.g. `("model", 0) => ("models", ...)`.
  `Context` carries `models`, `current_model` and MCP servers.
- `App::notice(text)` (app.rs:565) pushes a notice line into the transcript.

### Launcher (`crates/harness-cli`)

- `src/bin/mcpe.rs` — clap `Cli` with `-m/--model`, `-e/--effort`, `-w/--workspace`,
  `--mcp-config`, `--kb`, `--sessions-dir`, `--budget-usd`, `--models`, `--mock-script`, and an
  `after_help` "Defaults:" block. It builds `McpeArgs` and calls `commands::mcpe`.
- `src/launcher.rs` — constants (`MODEL_ENV = "MCPE_MODEL"`, `HOME_MCP_FILE =
  ".config/mcpe/mcp.json"`, `WORKSPACE_KB_FILE = ".mcpe/kb.json"`), `McpeArgs`, the
  `Lookup { var, is_file, cwd, home }` world struct, and the pure
  `resolve_mcpe(args, registry, lookup) -> TuiOptions`. Its tests build a fake `World`
  (launcher.rs:~165-200). Follow that pattern for config discovery tests.
- `src/commands.rs:161-183` — `TuiOptions { workspace, model, effort, models, mcp_config, kb,
  sessions_dir, budget_usd, mock_script, key_hint }`; `build_tui_session(options, env)` builds
  the `App`; `tui(options, err)` runs it; `mcpe(args, err)` (line 410) builds a real `Lookup`
  and calls `tui`.
- `crates/harness-tui/examples/snapshot.rs` prints seven screens as text; it accepts optional
  `WIDTH HEIGHT` arguments (lines ~27-35).

### Theme source data (verified on this machine, 2026-10-10)

Ghostty 1.3.1 is installed. Its bundled themes live in
`/Applications/Ghostty.app/Contents/Resources/ghostty/themes/` (232 files). Each is 22 lines of
`key = value`:

```
palette = 0=#1d2021
...
palette = 15=#ebdbb2
background = #1d2021
foreground = #ebdbb2
cursor-color = #ebdbb2
cursor-text = #1d2021
selection-background = #665c54
selection-foreground = #ebdbb2
```

Ghostty's theme collection is generated from the iTerm2-Color-Schemes project
(`https://github.com/mbadolato/iTerm2-Color-Schemes`). The bundle contains no licence file;
Step 1 verifies the upstream licence before anything is copied.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| All tests | `cargo test --workspace --all-features` | 0 failed; baseline before this plan is **634 passed** |
| TUI tests | `cargo test -p harness-tui --all-features` | 0 failed |
| CLI tests | `cargo test -p harness-cli --all-features` | 0 failed |
| Snapshot | `cargo run -q -p harness-tui --example snapshot -- 100 30 <theme>` | prints screens, exit 0 |
| Help | `cargo run -q -p harness-cli --bin mcpe -- --help` | exit 0 |

Apply `cargo fmt --all` yourself after editing (formatting is part of the gates).

## Scope

**In scope** (the only files you should create or modify):
- `crates/harness-tui/src/theme.rs` → becomes `crates/harness-tui/src/theme/mod.rs` (move it; there is no git history to preserve)
- `crates/harness-tui/src/theme/palette.rs` (create)
- `crates/harness-tui/src/theme/builtin.rs` (create)
- `crates/harness-tui/src/theme/config.rs` (create)
- `crates/harness-tui/themes/*.ghostty` (create; copied files, see Step 1)
- `crates/harness-tui/themes/THIRD_PARTY.md` (create)
- `crates/harness-tui/src/{app.rs,commands.rs,completion.rs,terminal.rs,lib.rs}`
- `crates/harness-tui/src/ui/picker.rs` and `crates/harness-tui/src/ui/help.rs` (only if a picker title or help row needs it)
- `crates/harness-tui/tests/*.rs` and `crates/harness-tui/tests/common/mod.rs` (new tests; mechanical updates only)
- `crates/harness-tui/examples/snapshot.rs`
- `crates/harness-cli/src/{launcher.rs,commands.rs,main.rs,bin/mcpe.rs}`
- `crates/harness-cli/tests/cli.rs`
- `README.md` (Usage/Configuration sections only)
- `plans/README.md` (status row only)

**Out of scope** (do NOT touch):
- `crates/harness-agent/**`, `crates/harness-core/**`, `configs/**`, `schemas/**`, other `plans/*.md`.
- The current default palette values in `Theme::default()`; the default theme stays Tokyo Night with exactly these values.
- Approval key handling, the drift accept gate, cost/budget logic, animations timing.
- Writing any user file. `/theme` changes the session only; it never writes `config.json`.
- Following the terminal's own theme automatically (reading Ghostty's config or querying the terminal background). Deferred.
- No new dependencies. `serde`, `serde_json` are already available to `harness-tui`; if you believe you need another crate, STOP.

## Git workflow

- Work in the current working tree. Do NOT create a branch or worktree.
- Do NOT commit or push (the repository has no commits; the owner commits).

## Design (read fully before Step 2)

### Theme model: base palette, then derived roles, then overrides

A Ghostty theme only defines a **base palette**: `background`, `foreground`, sixteen ANSI
colours `palette[0..=15]`, and optional `selection-background`. The TUI needs sixteen semantic
roles. Derive them with one fixed function so every theme, built-in or user-supplied, looks
consistent:

| Role | Derivation (`mix(a, b, t)` = existing `lerp`) |
|---|---|
| `bg` | `background` |
| `text` | `foreground`, then if `contrast(text, bg) < 4.5`: `ensure_contrast(text, bg, toward, 4.5)` with `toward` = black `#000000` when `luminance(bg) >= 0.5`, else white `#ffffff` (added 2026-10-10 by owner decision; Ghostty's Solarized Light text `#657b83` on `#fdf6e3` is 4.13) |
| `surface` | `mix(bg, text, 0.04)` |
| `raised` | `mix(bg, text, 0.07)` |
| `highlight` | `selection-background` if present **and** `contrast(text, it) >= 3.0`, else `mix(bg, text, 0.16)` |
| `border` | `mix(bg, text, 0.22)` |
| `muted` | `mix(bg, text, 0.55)` |
| `accent` | `palette[6]` (cyan) |
| `accent_alt` | `palette[5]` (magenta) |
| `user` | `palette[4]` (blue) |
| `assistant` | `palette[5]` (magenta) |
| `success` | `palette[2]` (green) |
| `warning` | `palette[3]` (yellow) |
| `error` | `palette[1]` (red) |
| `diff_add_bg` | `mix(bg, palette[2], 0.18)` |
| `diff_del_bg` | `mix(bg, palette[1], 0.18)` |

Then apply `ensure_contrast(role, bg, 3.0)` to `muted`, `accent`, `accent_alt`, `user`,
`assistant`, `success`, `warning`, `error`: while `contrast(role, bg) < 3.0`, move the role
toward `text` in steps of `t = 0.05` (`role = mix(original, text, t)`, `t` up to `1.0`). This
fixes light themes whose yellows are unreadable on cream backgrounds (for example Gruvbox
Light warning `#d79921` on `#fbf1c7`).

`contrast(a, b)` is the WCAG 2.x contrast ratio: for each colour, channel `c = v/255`,
linearise `c <= 0.04045 ? c/12.92 : ((c+0.055)/1.055)^2.4`, luminance
`L = 0.2126 R + 0.7152 G + 0.0722 B`; ratio `(L_hi + 0.05) / (L_lo + 0.05)`.

**Tokyo Night is special**: the built-in `tokyo-night` returns exactly `Theme::default()`'s
hand-tuned roles when the user gives **no base overrides**. If the user overrides a base key
while on `tokyo-night`, derive from the embedded `TokyoNight Night` base palette plus the
overrides, using the table above.

Finally, **semantic overrides** from config (see below) replace roles exactly, with no contrast
adjustment (the user's explicit choice wins).

`support` and `no_color` are never part of a theme selection: they always come from the
terminal (`ColorSupport::detect`, `NO_COLOR`).

### Built-in themes (20)

| Slug | Source file in Ghostty's theme directory | Extra accepted names |
|---|---|---|
| `tokyo-night` | (hand-tuned; base palette from `TokyoNight Night`) | `tokyonight`, `tokyonight-night` |
| `gruvbox-dark` | `Gruvbox Dark` | |
| `gruvbox-dark-hard` | `Gruvbox Dark Hard` | |
| `gruvbox-light` | `Gruvbox Light` | |
| `catppuccin-mocha` | `Catppuccin Mocha` | |
| `catppuccin-macchiato` | `Catppuccin Macchiato` | |
| `catppuccin-frappe` | `Catppuccin Frappe` | |
| `catppuccin-latte` | `Catppuccin Latte` | |
| `nord` | `Nord` | |
| `monokai` | `Monokai Classic` | `monokai-classic` |
| `monokai-pro` | `Monokai Pro` | |
| `dracula` | `Dracula` | |
| `one-dark` | `Atom One Dark` | `atom-one-dark` |
| `solarized-dark` | `iTerm2 Solarized Dark` | `iterm2-solarized-dark` |
| `solarized-light` | `iTerm2 Solarized Light` | `iterm2-solarized-light` |
| `rose-pine` | `Rose Pine` | |
| `rose-pine-moon` | `Rose Pine Moon` | |
| `rose-pine-dawn` | `Rose Pine Dawn` | |
| `everforest-dark` | `Everforest Dark Hard` | `everforest-dark-hard` |
| `kanagawa-wave` | `Kanagawa Wave` | |

That is 20 files (19 rows with a source file plus `TokyoNight Night` for the Tokyo Night base).

**Name matching**: normalise by trimming, lowercasing, and turning every run of spaces,
underscores or hyphens into one `-`. So `Gruvbox Dark Hard`, `gruvbox_dark_hard` and
`gruvbox-dark-hard` all match. Match the slug first, then the extra names. Unknown names are an
error that lists every slug.

### `config.json` format

Locations, lowest to highest precedence:
1. `~/.config/mcpe/config.json` (global; matches the existing `~/.config/mcpe/mcp.json` convention)
2. `<workspace>/.mcpe/config.json` (workspace)
3. `--config <path>` when given (replaces both discovered files; it is the only file read)

Example (document this in the README):

```json
{
  "theme": "gruvbox-dark-hard",
  "colors": {
    "background": "#1d2021",
    "foreground": "#ebdbb2",
    "selection-background": "#665c54",
    "palette": ["1=#fb4934", "4=#83a598"],
    "accent": "#ff5555",
    "border": "#504945",
    "diff-add-bg": "#203020"
  }
}
```

- `theme` (string): a built-in name, matched as above.
- `theme-file` (string): path to any Ghostty-format theme file (for example one of Ghostty's
  232 bundled themes). Relative paths resolve against the directory of the `config.json` that
  set it; a leading `~/` expands to the home directory. When both `theme` and `theme-file` are
  set in the effective config, `theme-file` wins and a warning says `theme` was ignored.
- `colors` (object), keys:
  - **Base keys** (change the palette before derivation, Ghostty names): `background`,
    `foreground`, `selection-background`, and `palette` as an array of Ghostty-syntax strings
    `"N=#rrggbb"` with `N` in `0..=15`.
  - **Accepted and ignored silently** (so a pasted Ghostty theme block does not warn):
    `cursor-color`, `cursor-text`, `selection-foreground`.
  - **Semantic keys** (replace a final role exactly, kebab-case of the `Theme` fields): `bg`,
    `surface`, `raised`, `highlight`, `border`, `text`, `muted`, `accent`, `accent-alt`,
    `user`, `assistant`, `success`, `warning`, `error`, `diff-add-bg`, `diff-del-bg`.
- Colour values: `#rrggbb`, `rrggbb`, or `#rgb` (expanded per digit), case-insensitive.
- Merging: workspace keys override global keys one by one (`theme`, `theme-file`, and each
  `colors` key independently; `palette` entries merge by index).

**Theme selection precedence** (highest first): `--theme` flag, `MCPE_THEME` environment
variable, effective config `theme-file`/`theme`, built-in `tokyo-night`. Colour overrides from
config apply on top of **whatever** theme is selected (including after `/theme` switches),
like Ghostty.

**Errors**:
- Discovered config files never block startup. Invalid JSON, unknown keys, bad colour values,
  bad palette indexes, an unknown `theme` name and an unreadable `theme-file` each produce one
  warning string naming the file and the key; the bad item is skipped. Warnings are shown as
  transcript notices when the TUI opens.
- Explicit inputs fail fast before the terminal opens, with exit code 1, like a bad `-m`:
  `--config` pointing at a missing or unparseable file, and an unknown `--theme` or
  `MCPE_THEME` name (message lists the valid slugs).

### `/theme` command

- `/theme` opens the picker listing every built-in (label = slug, detail = `dark`/`light`
  decided by `luminance(bg) < 0.5`), plus a first row `custom` when a `theme-file` is active.
  The current theme is preselected.
- Moving the selection **previews** the theme immediately (re-resolve and assign `app.theme`).
- Enter keeps the previewed theme. Esc or `q` restores the theme that was active when the
  picker opened.
- `/theme <name>` switches directly; unknown names give an error notice listing slugs.
- Usage string `usage: /theme [name]`. Add a `COMMAND_HELP` row:
  `("/theme [name]", "pick a colour theme (this session; set it permanently in config.json)")`.
- Autocomplete: `/theme ` completes theme slugs (popup heading `themes`).
- `/theme` is allowed while a turn or drift run is in progress (it is purely visual). Do not
  call `refuse_while_blocked` for it.
- Preview and switching keep the terminal's `support` and `no_color`.

## Steps

### Step 0: Baseline

Run the three gate commands.

**Verify**: fmt exit 0; clippy exit 0; `cargo test --workspace --all-features` reports 634
passed, 0 failed. If the count differs, record the actual number and continue only if 0 failed.

### Step 1: Verify the licence and copy the theme files

1. Fetch the upstream licence. Use the `tinyfish` CLI if available
   (`tinyfish fetch https://raw.githubusercontent.com/mbadolato/iTerm2-Color-Schemes/master/LICENSE`),
   otherwise `curl -fsSL` of the same URL. Read it.
   - If it is MIT or another permissive licence that allows redistribution with attribution, continue.
   - If it is not permissive, or cannot be fetched, STOP and report.
2. Create `crates/harness-tui/themes/` and copy the 20 source files from
   `/Applications/Ghostty.app/Contents/Resources/ghostty/themes/` **byte for byte** with
   `cp`, renaming to `<slug>.ghostty` (Tokyo Night's base file becomes `tokyo-night.ghostty`).
   Never retype colour values by hand.
3. Write `crates/harness-tui/themes/THIRD_PARTY.md`: source (Ghostty 1.3.1 bundled themes,
   generated from iTerm2-Color-Schemes), upstream URL, licence name, the full licence text you
   fetched, and the mapping table slug → original filename.

**Verify**:
- `ls crates/harness-tui/themes/*.ghostty | wc -l` → `20`
- For each file, `cmp "<ghostty dir>/<original name>" crates/harness-tui/themes/<slug>.ghostty`
  → no output (write a small shell loop over the mapping table).
- `grep -c '^palette = ' crates/harness-tui/themes/*.ghostty` → `16` for every file.

### Step 2: Split the theme module and add the palette layer

1. Move `crates/harness-tui/src/theme.rs` to `crates/harness-tui/src/theme/mod.rs`
   unchanged, and add `mod palette; mod builtin; mod config;` with the public re-exports you
   need. The existing public paths (`crate::theme::{Rgb, Theme, ColorSupport, lerp, dim,
   color_to_rgb}`) must keep working; do not edit any `ui/*.rs` import.
2. In `theme/palette.rs` add:
   - `pub fn parse_hex(s: &str) -> Option<Rgb>` (formats in the Design section).
   - `pub struct BasePalette { pub background: Rgb, pub foreground: Rgb, pub palette: [Rgb; 16], pub selection_background: Option<Rgb> }`.
   - `pub fn parse_ghostty(text: &str) -> Result<BasePalette, String>`: ignore blank lines and
     lines starting with `#`; split on the first `=`, trim both sides; handle `palette = N=#hex`;
     accept and ignore any other key (Ghostty files may carry cursor keys). Error when
     `background`, `foreground` or any of the 16 palette entries is missing, or a recognised
     key has a bad colour; the message includes the 1-based line number.
   - `pub fn contrast(a: Rgb, b: Rgb) -> f32`, `pub fn luminance(c: Rgb) -> f32`,
     `pub fn ensure_contrast(c: Rgb, bg: Rgb, toward: Rgb, min: f32) -> Rgb`.
   - `pub fn derive(base: &BasePalette) -> Theme` implementing the Design table, returning a
     `Theme` with `support: TrueColor, no_color: false`.
3. In `theme/mod.rs` add `impl Theme { pub fn with_terminal(self, env: &dyn Fn(&str) -> Option<String>) -> Self }`
   that sets only `support` and `no_color` (same rules as `detect`). Re-implement
   `Theme::detect(env)` as `Theme::default().with_terminal(env)` so its behaviour is unchanged.

**Verify**: `cargo test -p harness-tui --all-features` → 0 failed (all old tests still pass);
new unit tests from the Test plan for `parse_hex`, `parse_ghostty`, `contrast`,
`ensure_contrast`, `derive` and `with_terminal` pass.

### Step 3: Built-in registry

In `theme/builtin.rs`:
- A static table of `(slug, extra_names, file_text)` with `file_text = include_str!("../../themes/<slug>.ghostty")`.
- `pub fn names() -> Vec<&'static str>` (slugs in the Design table order, `tokyo-night` first).
- `pub fn normalise(name: &str) -> String`.
- `pub fn find(name: &str) -> Option<&'static Builtin>` (slug, then extra names).
- `pub fn base(builtin) -> BasePalette` (parse; a parse failure of an embedded file is a bug:
  `expect` with the slug in the message is acceptable here, and a test covers every file).
- `pub fn theme(builtin) -> Theme`: `Theme::default()` for `tokyo-night`, else `derive(&base)`.

**Verify**: `cargo test -p harness-tui --all-features theme` → new tests pass, including the
contrast test over all 20 built-ins (Test plan).

### Step 4: Config parsing and resolution (pure)

In `theme/config.rs`:
- `pub struct ConfigSource { pub path: PathBuf, pub text: String }` (already read by the caller).
- `pub struct ThemeChoice { pub flag: Option<String>, pub env: Option<String> }` (`--theme`, `MCPE_THEME`).
- `pub struct ThemeSpec` — the resolved, reusable selection: the chosen base (`Builtin` slug or
  a parsed custom `BasePalette` with its label), the merged base overrides, and the merged
  semantic overrides. It must be `Clone` and `Debug`.
- `pub fn load(sources: &[ConfigSource], choice: &ThemeChoice, read: &dyn Fn(&Path) -> Result<String, String>, home: Option<&Path>) -> Result<(ThemeSpec, Vec<String>), String>`:
  - Parse each source with `serde_json::Value` (not a strict struct), so unknown keys can be
    reported as warnings instead of failing.
  - Return `Err` only for an unknown `flag`/`env` theme name. Everything from config files is a
    warning, per the Design section.
  - `read` is used only for `theme-file`.
- `impl ThemeSpec { pub fn resolve(&self) -> Theme; pub fn with_builtin(&self, slug) -> Option<ThemeSpec>; pub fn label(&self) -> String }`
  — `resolve` applies: base theme → base overrides → derive (or the Tokyo Night special case)
  → semantic overrides. The returned `Theme` has `TrueColor`/`no_color: false`; callers apply
  `with_terminal`.

**Verify**: `cargo test -p harness-tui --all-features config` → the config tests from the Test
plan pass.

### Step 5: Wire the theme into the TUI

1. `App`: add `pub theme_spec: ThemeSpec` (default: built-in `tokyo-night`, no overrides) and a
   private field holding the theme spec saved when a theme picker opened.
   Add `pub fn set_theme_spec(&mut self, spec: ThemeSpec)` that assigns `theme_spec` and sets
   `theme = spec.resolve()` with the current `theme.support` and `theme.no_color` kept.
2. `terminal.rs:250`: replace the overwrite with
   `app.theme = app.theme.clone().with_terminal(&|name| std::env::var(name).ok());`.
3. Add a picker kind: `pub enum PickerKind { Model, Theme }` and a `kind` field on
   `PickerView`. Model picker behaviour stays identical. For `Theme`, selection moves call the
   preview, Enter keeps, Esc/q restore (Design section).
4. `commands.rs`: `SlashCommand::Theme(Option<String>)`, parse `/theme` and `/theme <name>`
   (more than one argument → `usage: /theme [name]`), add the `COMMAND_HELP` row.
5. `completion.rs`: `("theme", 0)` completes built-in slugs with heading `themes`; extend
   `is_complete` so `/theme <valid slug>` counts as complete.
6. Startup warnings: add `pub fn push_notices(&mut self, notices: Vec<String>)` (or reuse
   `notice`) so the CLI can hand config warnings to the app before `run`.

**Verify**: `cargo test -p harness-tui --all-features` → 0 failed, including the new reducer,
completion and render tests from the Test plan.

### Step 6: CLI and launcher

1. `TuiOptions`: add `theme: ThemeSpec`-producing inputs. Recommended shape:
   `pub theme: Option<String>` (the `--theme` flag), `pub config: Option<PathBuf>` (`--config`),
   and `pub config_sources: Vec<PathBuf>` (discovered files). `build_tui_session` reads the
   files, calls `theme::config::load`, sets `app.set_theme_spec(spec)` and pushes warnings as
   notices. An `Err` from `load` (or a missing/unparseable `--config`) returns the error string
   so `tui` prints `error: ...` and exits 1 before the terminal opens.
2. `launcher.rs`: constants `THEME_ENV = "MCPE_THEME"`, `HOME_CONFIG_FILE = ".config/mcpe/config.json"`,
   `WORKSPACE_CONFIG_FILE = ".mcpe/config.json"`. `McpeArgs` gains `theme` and `config`.
   `resolve_mcpe` fills `config_sources` with the existing files in precedence order (global
   then workspace) unless `--config` is given, and passes `MCPE_THEME` through (flag wins).
3. `bin/mcpe.rs`: `--theme <name>` and `--config <path>` flags with doc comments in the
   existing style; extend `after_help`'s Defaults block with
   `theme       $MCPE_THEME, else "theme" in config.json, else tokyo-night` and
   `config      ~/.config/mcpe/config.json, then <workspace>/.mcpe/config.json (workspace wins)`,
   and an example line `mcpe --theme gruvbox-dark-hard`.
4. `main.rs` (`harness tui`): add `--theme <name>` only (no config discovery for this command).
5. README: in Usage add the two flags; add a "Themes and colours" subsection under
   Configuration with the slug list, file locations, precedence, the JSON example and the key
   list. Do not use em dashes.

**Verify**:
- `cargo test -p harness-cli --all-features` → 0 failed including new launcher/CLI tests.
- `cargo run -q -p harness-cli --bin mcpe -- --help | grep -c -- "--theme\|--config"` → `2` or more.
- `cargo run -q -p harness-cli --bin mcpe -- --theme nope --mock-script fixtures/agent/hello </dev/null` → exit 1, stderr names `nope` and lists `gruvbox-dark-hard`.

### Step 7: Snapshot support and final gates

1. `examples/snapshot.rs`: accept an optional third argument, a theme name, resolved through
   `builtin::find` (exit 2 with the slug list when unknown). Print the theme label in each
   screen header.
2. Run the final gates.

**Verify**:
- `cargo run -q -p harness-tui --example snapshot -- 100 30 gruvbox-dark-hard` → exit 0.
- `cargo run -q -p harness-tui --example snapshot -- 100 30 catppuccin-latte` → exit 0.
- fmt exit 0; clippy exit 0; `cargo test --workspace --all-features` → 0 failed, count > 634.
- Run `cargo test -p harness-tui --all-features --test render` 10 times in a loop → 0 failures.

## Test plan

Unit tests inside `theme/palette.rs`, `theme/builtin.rs`, `theme/config.rs` (style: the
existing `mod tests` in `theme.rs` with `env_of`):
- `parse_hex`: `#1d2021`, `1D2021`, `#fff` → `Rgb(255,255,255)`; rejects `#12345`, `zzzzzz`, empty.
- `parse_ghostty`: parses a full Ghostty file; ignores comments, blank lines and cursor keys;
  missing `palette = 7` → error naming it; bad colour → error with the line number.
- `contrast`: black/white → 21.0 (±0.01); identical colours → 1.0.
- `ensure_contrast`: Gruvbox Light warning `#d79921` on `#fbf1c7` ends with contrast ≥ 3.0; a
  colour already passing is unchanged.
- `with_terminal`: keeps every colour role, changes only `support`/`no_color`; `NO_COLOR=1`
  still resolves everything to `Color::Reset`.
- Every built-in: embedded file parses; `theme(b)` has `contrast(text, bg) >= 4.5` and
  `contrast(role, bg) >= 3.0` for `muted, accent, accent_alt, user, assistant, success, warning, error`.
- `builtin::theme(tokyo-night) == Theme::default()` (guards existing render tests).
- `solarized-light`: derived `text` differs from `#657b83` and has `contrast(text, bg) >= 4.5`; for
  every built-in whose Ghostty foreground already passes 4.5, derived `text` equals that foreground exactly.
- Ghostty drift guard: for each built-in, if the original Ghostty file exists on this machine,
  its bytes equal the embedded file; skip (with an `eprintln!`) when Ghostty is not installed.
- Name matching: `Gruvbox Dark Hard`, `gruvbox_dark_hard`, `  GRUVBOX-dark-hard ` all find
  `gruvbox-dark-hard`; `Monokai Classic` finds `monokai`; `Atom One Dark` finds `one-dark`.
- Config: global sets `theme`, workspace overrides it; workspace `colors.accent` overrides
  global; `palette` entries merge by index; base override `background` changes derived
  `surface`; semantic override `accent` is exact (no contrast adjustment); `#rgb` accepted;
  unknown key, bad hex, `palette` index 16, invalid JSON, unknown `theme` → one warning each
  naming the file, and the rest of the file still applies; `theme-file` relative path resolves
  against the config's directory and `~/` against `home`; `theme` + `theme-file` → warning and
  `theme-file` wins; `--theme` beats `MCPE_THEME` beats config; unknown `--theme` → `Err`
  listing slugs; colour overrides still apply after `with_builtin` (theme switch).

Reducer and render tests in `crates/harness-tui/tests/` (pattern: `tests/reducer.rs`,
`tests/render.rs`, `tests/completion.rs`):
- `/theme` opens a picker with 20 rows (21 with a custom theme-file) and the current theme selected.
- Down in the theme picker changes `app.theme.bg` to the next theme's bg; Esc restores the
  original `Theme` exactly; Enter keeps the previewed one.
- `/theme nord` switches; `/theme nope` adds an error notice listing slugs; `/theme a b` → usage.
- `/theme` works while a turn is running (no refusal notice).
- Model picker behaviour is unchanged (existing tests pass untouched).
- `/the` autocompletes to `/theme`; `/theme gru` suggests the three gruvbox slugs.
- Render: with `gruvbox-dark-hard` at 100x30, the frame background cell colour equals
  `Color::Rgb(0x1d,0x20,0x21)`; with `Indexed256` support, cells use `Color::Indexed(_)` only.

CLI tests (pattern: the `World` tests in `launcher.rs`, `tests/cli.rs`):
- Discovery: only global exists → sources `[global]`; both → `[global, workspace]`; `--config`
  → `[that file]` only.
- `MCPE_THEME` passes through; `--theme` overrides it.
- Real binary: unknown `--theme` exits 1 before the terminal opens; missing `--config` file
  exits 1; a discovered config with invalid JSON does not prevent session building and yields a
  notice (test through `build_tui_session`).

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` exits 0
- [ ] `cargo test --workspace --all-features` exits 0 with more than 634 passed
- [ ] `ls crates/harness-tui/themes/*.ghostty | wc -l` prints `20`, and every file is byte-identical to its Ghostty original (Step 1 loop prints nothing)
- [ ] `crates/harness-tui/themes/THIRD_PARTY.md` exists and contains the upstream licence text
- [ ] `grep -n "from_process_env" crates/harness-tui/src/terminal.rs` prints nothing (the overwrite is gone)
- [ ] `grep -rn "Rgb(0x1a, 0x1b, 0x26)" crates/harness-tui/src/theme/mod.rs` still matches (default palette unchanged)
- [ ] `cargo run -q -p harness-cli --bin mcpe -- --help` lists `--theme` and `--config`
- [ ] `cargo run -q -p harness-tui --example snapshot -- 100 30 nord` exits 0
- [ ] No files outside the Scope list were created, modified or deleted
- [ ] No new entries in any `[dependencies]` table
- [ ] `plans/README.md` status row updated (unless the reviewer said they maintain it)

## STOP conditions

Stop and report back (do not improvise) if:
- The Ghostty theme directory or any of the 20 source files is missing.
- The upstream licence cannot be fetched or is not permissive.
- An embedded built-in still fails the contrast test **for `text` vs `bg`** after the text rule
  in the Design table (owner decided 2026-10-10: darken/lighten text generically). Failures of the other roles must be fixed by the
  derivation rules, not by per-theme hacks; if the rules cannot fix one, STOP.
- Keeping `Theme::default()` identical would break with your design.
- Making `/theme` work seems to require touching approval handling, the agent bridge, or any
  out-of-scope crate.
- A step's verification fails twice after a reasonable fix attempt.
- You think a new dependency is needed.

## Maintenance notes

- New built-in: copy one more Ghostty file byte for byte, add a table row and a
  `THIRD_PARTY.md` mapping row; the contrast and drift-guard tests cover it automatically.
- The role derivation is one function (`palette::derive`). Tuning it changes every built-in
  except Tokyo Night at once; review snapshot dumps of a dark and a light theme after any change.
- Semantic overrides bypass contrast adjustment by design. If users report unreadable custom
  configs, consider a warning (not an adjustment).
- Deferred: `/theme save` writing `config.json`; following the terminal's theme automatically;
  light/dark auto-selection from the terminal background.
- Reviewer focus: that `terminal::run` keeps the palette, that Esc in the theme picker restores
  the exact previous theme, and that config problems never stop startup while explicit
  flags fail fast.
