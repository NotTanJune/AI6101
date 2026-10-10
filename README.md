# MCP tool-knowledge adaptation harness

A Rust terminal application (Ratatui) and headless CLI that studies whether updating only the
stale parts of an agent's tool knowledge restores task performance after MCP servers change,
while preserving the tools that still work.

It compares three strategies against the same Tool KB (the JSON knowledge base of tool cards):

- `no_update`: keep the original Tool KB.
- `full_rewrite`: regenerate the whole Tool KB from the target server version.
- `selective_patch`: update only cards and fields tied to detected changes. Unaffected cards
  stay byte-identical.

Server versions are Early, Middle, and Late (for the supplied benchmark: `original`, `iter2`, `iter4`). Changes are classified at Tool, Parameter, and
Description level. A canary (a small probe against the recent patch) gates promotion of a
candidate KB. Benchmark execution, the completion verifier, and scoring live in a separate
Python workstream; this repository owns the Rust side and the shared interchange contracts.

Project scope, steps, and done criteria: `plans/001-mcp-harness-tui.md`.
Experiment protocol and blocked items: `docs/experiment-protocol.md`.

## Status (read before relying on results)

- The harness runs offline against checked-in smoke fixtures with a deterministic mock
  provider. No network access and no paid calls are used in tests or the smoke run.
- The `mcpevol` adapter reads the supplied MCPEvol-Bench checkout (commit
  `888c779a0fe735f612ca62bdd1fdd7c9e6bf8701`) read-only. The protocol records the seven input
  files with SHA-256 values, the counts (163 servers, 1,685 tools, 393 test cases, 201 selected
  tasks), the stage mapping, and the stage by change-class coverage matrix.
- The adapter reconstructs versions from a snapshot. `status_dict.json` is a snapshot of live
  servers that already contains most structural evolutions, so `original`, `iter1`, and `iter2`
  are reconstructions, not live server definitions. Free-text constraint changes and optimized
  description text are not reproduced. Details and limits are in section 1.5 of
  `docs/experiment-protocol.md`.
- Under the stage mapping below, Early holds only unchanged control tasks (15 of the 201).
- PROPOSED, NOT FROZEN: a 20-task pilot (`cargo run -q -p harness-core --example
  pilot_candidates`). The project owner reviews the no-update cost before it is frozen.
- BLOCKED (contract not agreed): the Python runner argv, environment, and exit codes are a
  PROPOSAL, not yet agreed with the Python workstream owner. Live integration with the Python
  runner has not been done. Runs use the fixture runner.
- TO BE DECIDED BY PROJECT OWNER: base model, decoding, judge version, repetitions (1 or 3),
  rate card, cap, and the frozen pilot task set. No real-provider pilot has been run.

## Workspace layout

| Path | Contents |
|------|----------|
| `crates/harness-core` | Library: Tool KB model, config, events, cost ledger, redaction, diffing, strategies, patches, providers, canary, runner bridge, pipeline |
| `crates/harness-tui` | Library: Ratatui state, reducer, screens, terminal lifecycle |
| `crates/harness-cli` | Binary `harness` with commands `tui`, `validate`, `run` |
| `schemas/` | `tool-kb.schema.json` and `run-event.schema.json` (JSON Schema draft 2020-12) |
| `fixtures/` | Offline smoke inputs (`fixtures/smoke/`) and contract fixtures (`fixtures/contracts/`) |
| `configs/` | Example experiment configuration (`configs/example.toml`) |
| `crates/harness-core/examples/` | `pilot_candidates`: coverage matrix and proposed pilot over the MCPEvol-Bench inventory |
| `docs/` | Experiment protocol (`docs/experiment-protocol.md`) |
| `plans/` | Implementation plan |

## Prerequisites

- Rust 1.98.0 with `rustfmt` and `clippy`. The repository pins this in `rust-toolchain.toml`,
  so with [rustup](https://rustup.rs) installed the right toolchain is fetched automatically on
  the first `cargo` command.
- A terminal for the TUI.
- Nothing else for the offline smoke run. No Python is required by this repository.

## Build and verify

Run from the repository root. All three gates must exit 0.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Then the command-line checks:

```sh
cargo run -p harness-cli -- --help
cargo run -p harness-cli -- validate --config fixtures/smoke.toml
cargo run -p harness-cli -- run --config fixtures/smoke.toml --output target/harness-smoke
```

## Usage

```text
mcpe [-m <model>] [-e <effort>] [-w <directory>] [--mcp-config <path>] [--kb <path>] [--theme <name>] [--config <path>]
harness tui [--workspace <directory>] [--model <alias>] [--effort <level>] [--mcp-config <path>] [--theme <name>]
harness validate --config <path>
harness run --config <path> --output <directory>
```

- `mcpe` opens the interactive coding agent in the current directory. It needs no arguments.
  Install it once with `cargo install --path crates/harness-cli --bin mcpe`, then run `mcpe`
  from any project. Defaults:
  - Model: `$MCPE_MODEL`, else the first of `luna`, `flash`, `sol`, `pro`, `astra` whose API key
    variable is set (`OPENAI_API_KEY` for luna, sol and astra; `GEMINI_API_KEY` for flash and
    pro). With no key set it says which variables to set.
  - Workspace: the current directory (`-w <directory>` changes it).
  - MCP servers: `<workspace>/.mcp.json`, else `<workspace>/mcp.json`, else
    `~/.config/mcpe/mcp.json` (the first that exists). Override with `--mcp-config`.
  - Tool KB: `<workspace>/.mcpe/kb.json` when it exists (`--kb` overrides).
  - Session transcripts: `<workspace>/.mcpe/sessions` (`--sessions-dir` overrides).
  - Model registry: built into the binary (`--models <file>` replaces it).
  - Theme: `--theme <name>`, else `$MCPE_THEME`, else `theme` in config.json, else `tokyo-night`.
  - Config files: `~/.config/mcpe/config.json`, then `<workspace>/.mcpe/config.json` (see
    [Themes and colours](#themes-and-colours)). `--config <path>` replaces both.

  Examples: `mcpe`, `mcpe -m flash -e high`, `mcpe --mcp-config ./servers.json`,
  `mcpe --theme gruvbox-dark-hard`.
  Inside, type `/` for command suggestions (Tab completes, Up and Down choose, Esc dismisses),
  `/help` for all keys and commands, and Alt+Enter for a new line. Colours follow the terminal:
  true colour, 256 colours or 16 colours are detected from `COLORTERM` and `TERM`, and
  `NO_COLOR` turns colours off.
- `tui` is the same agent through the `harness` binary, with the long flag names. It also uses
  the built-in model registry, so it starts from any directory. Its sessions go to
  `<workspace>/.harness/sessions` unless `--sessions-dir` is given. It accepts `--theme` but
  reads no config files.
- `validate` checks the configuration and inputs and prints the strategy set and schema versions. It contacts no provider and no live MCP server. Exit 0 when valid, 1 when invalid (problems listed on stderr).
- `run` runs the configured strategy, version, task, and repetition matrix headlessly through the
  same core pipeline and writes the output layout below. Exit codes: 0 ok, 1 setup or I/O error, 2 usage error, 3 run failed, 4 cancelled, 5 local dispatch budget exhausted.

## Using the real MCPEvol-Bench data

The benchmark is <https://github.com/Octobrist/MCPEvol-Bench>. Clone it outside this repository.
Its data lives in `src/custom/data`. The adapter reads two directories (here the same one) from
environment variables, never from a path in a file:

```sh
export MCP_DATA_DIR=<benchmark clone>/src/custom/data
export EVAL_DATA_DIR=<benchmark clone>/src/custom/data
cargo run -q -p harness-cli -- validate --config fixtures/mcpevol-real.example.toml
cargo run -q -p harness-core --example pilot_candidates
```

`validate` prints the input hashes, counts, name resolution, and unmapped items. The example
prints the coverage matrix and the proposed pilot ids. Neither contacts a provider or a server.

Configuration for this data (see `fixtures/mcpevol-real.example.toml`):

- `[inputs] adapter = "mcpevol"` with `catalog_env` and `eval_env` naming the two variables.
- Version labels: `original`, `iter1`, `iter2`, `iter3`, `iter4`. `iterN` applies evolution
  files 1 to N cumulatively.
- Stage mapping, a project decision: Early = `original` (depth 0), Middle = `iter2` (depth 2),
  Late = `iter4` (depth 4). Set it in `[versions.stages]`. Task ids are the `idx` values listed
  in `chosen_idx.json`.
- The checked-in invented miniature `fixtures/mcpevol-mini/` has the same file layout, so the
  adapter and its tests run offline without the benchmark.

Do not copy benchmark files into this repository.

## Configuration

Experiment settings are TOML. Start from `configs/example.toml`, which is fully commented and
defaults to the mock provider and the fixture runner. Sections: `[run]`, `[inputs]`,
`[versions]`, `[tasks]`, `[provider]`, `[judge]`, `[budget]` (with an optional rate card), and
`[runner]`. Unknown keys are rejected. Relative paths resolve against the config file's
directory. Paid providers (`openai`, `gemini`) require an `api_key_env` name, a
`max_output_tokens`, and a rate card entry; without a rate, paid calls are blocked.

`configs/example.toml` uses placeholders for task IDs and paths and leaves rate-card prices for
the owner to fill in. It runs offline on the smoke fixtures. For the supplied benchmark, start from
`fixtures/mcpevol-real.example.toml` instead.

### Themes and colours

The `mcpe` TUI ships 20 built-in themes: `tokyo-night` (the default), `gruvbox-dark`,
`gruvbox-dark-hard`, `gruvbox-light`, `catppuccin-mocha`, `catppuccin-macchiato`,
`catppuccin-frappe`, `catppuccin-latte`, `nord`, `monokai`, `monokai-pro`, `dracula`, `one-dark`,
`solarized-dark`, `solarized-light`, `rose-pine`, `rose-pine-moon`, `rose-pine-dawn`,
`everforest-dark` and `kanagawa-wave`. Names ignore case, and spaces or underscores work in place
of hyphens, so `Gruvbox Dark Hard` finds `gruvbox-dark-hard`. The built-in colours are copies of
the themes bundled with Ghostty (MIT licence; see `crates/harness-tui/themes/THIRD_PARTY.md`).

Choose a theme with `mcpe --theme <name>`, the `MCPE_THEME` variable, or a `theme` key in a
config file. Inside the TUI, `/theme` opens a picker that previews each theme as you move the
selection. Enter keeps the theme and Esc restores the previous one. `/theme <name>` switches
directly. Switches last for the session only; nothing writes a config file.

Config files, lowest precedence first:

1. `~/.config/mcpe/config.json`
2. `<workspace>/.mcpe/config.json`

Keys set in the workspace file override the same keys in the global file. `--config <path>`
replaces both files. A config file looks like this:

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

- `theme`: a built-in name, as above.
- `theme-file`: the path to any Ghostty theme file. A relative path resolves against the directory
  of the config file that sets it, and `~/` resolves against your home directory. When both
  `theme` and `theme-file` are set, `theme-file` wins and a warning says `theme` was ignored.
- `colors`, base keys: `background`, `foreground` and `selection-background` replace the theme's
  base colours. `palette` is an array of `"N=#rrggbb"` strings with N from 0 to 15. The other
  roles are derived from the base palette.
- `colors`, role keys: `bg`, `surface`, `raised`, `highlight`, `border`, `text`, `muted`, `accent`,
  `accent-alt`, `user`, `assistant`, `success`, `warning`, `error`, `diff-add-bg` and
  `diff-del-bg` set one role exactly, with no contrast adjustment.
- `colors`, ignored keys: `cursor-color`, `cursor-text` and `selection-foreground` are accepted
  and ignored, so a pasted Ghostty theme block does not warn.

Colour values are `#rrggbb`, `rrggbb` or `#rgb`, in any letter case. A problem in a config file
never stops the TUI. Each problem shows as one notice when the TUI opens, and the rest of the file
still applies. An explicit `--config` that is missing or not valid JSON, or an unknown `--theme`
or `MCPE_THEME` name, exits with status 1 before the terminal opens. The colour depth still comes
from `COLORTERM` and `TERM`, and `NO_COLOR` still turns colours off.

## Output layout

`harness run --output <dir>` writes:

| Path | Content |
|------|---------|
| `manifest.toml` | Resolved config without secrets, input checksums, contract versions |
| `events.jsonl` | Rust events for all strategies plus Python-source events in the order received |
| `python/<strategy>-rep<N>.jsonl` | Raw runner lines, verbatim |
| `summary.json` | Per strategy: adaptation status, canary status, verdict counts, metric events seen, cost totals including mock and unknown |
| `kb/` | Tool KB files handed to the runner |

## Schemas and fixtures

- `tool-kb/1`: the Tool KB JSON (`schemas/tool-kb.schema.json`).
- `run-event/1`: one JSON event per JSONL line (`schemas/run-event.schema.json`). Event types:
  `config`, `adaptation_diff`, `adaptation_outcome`, `model_call`, `mcp_action`,
  `canary_result`, `verifier_probe`, `verifier_verdict`, `metric_score`, `failure_category`,
  `error`, `run_complete`. Details are in `docs/experiment-protocol.md`.
- `fixtures/contracts/` holds valid and deliberately malformed examples used by the contract
  tests. `fixtures/smoke/` holds the one-server, three-version, one-task smoke inputs.

## Environment variables

| Name | Purpose |
|------|---------|
| `MCP_DATA_DIR` | Directory with the catalog files, the benchmark's `src/custom/data`. Not committed. |
| `EVAL_DATA_DIR` | Directory with the evaluation files, the same `src/custom/data`. Not committed. |
| Provider key variable | The config names the variable in `api_key_env` (for example `OPENAI_API_KEY` or `GEMINI_API_KEY`). Export its value in your shell. The value is never written to a file. |

The names of the two data variables are configurable via `[inputs].catalog_env` and
`[inputs].eval_env`.

## Security

- No secrets are stored. Config files hold environment variable names only, and validation
  rejects values that look like secrets.
- Secrets are never passed in argv. The process runner is spawned with an argument array (no
  shell) and receives credentials only through named environment variables.
- Events are redacted at creation, before they are written or displayed.
- In the TUI, credentials are entered through masked input, kept in memory, and never rendered,
  saved, or logged.
- The budget cap is a local dispatch cap, not the provider's bill. Unknown cost is reported as
  unknown, never as zero.
- `.gitignore` excludes `.env*`, `*.key`, `*.pem`, and `target/`. Do not commit the supplied
  datasets.

See `CONTRIBUTING.md` for the development workflow.
