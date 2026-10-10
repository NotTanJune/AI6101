# Plan 003: Rewrite `harness-tui` as an interactive coding-agent TUI

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update the status row for this plan in
> `plans/README.md`, unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: confirm plan 002 is marked DONE in
> `plans/README.md` and that `crates/harness-agent` exists and passes
> `cargo test -p harness-agent --all-features`. Run `git status --short` and
> `git log --oneline -5`; if commits exist, run
> `git diff --stat <first commit>..HEAD -- crates/harness-tui crates/harness-cli/src`.
> Compare the excerpts below with the live files. On a mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED (large UI rewrite; async/sync bridge; approval UX is safety-relevant)
- **Depends on**: plans/002-harness-agent-core.md
- **Category**: direction
- **Planned at**: no commit SHA (unborn `main`, all files untracked), 2026-10-04; refreshed 2026-10-05 against the plan-002 API

## Why this matters

The current TUI is an experiment console: screens for config, inventory, diff, run, canary and
results. The project owner rejected that design on 2026-10-04. The TUI must be a coding agent:
the user chats with a model, the model calls built-in coding tools and MCP tools, risky actions
need approval, and the user can switch model and reasoning effort, manage MCP servers, inspect
the Tool KB, and handle drift between a server's live tools and the KB (run a strategy plus the
canary, then accept or reject). All agent logic lives in `harness-agent` (plan 002); this plan
is the front end only.

## Current state

- `crates/harness-tui/src/lib.rs` modules: `app` (state + `Msg`→`Effect` reducer), `controller`
  (`RunController` trait), `diffview`, `form`, `inventory`, `runmodel`, `secret`, `terminal`
  (Crossterm loop, `TerminalGuard`, `drive`, `run`), `ui/{canary,config,diff,inventory,labels,mod,results,run}.rs`.
- `crates/harness-tui/src/app.rs:25-38`: `enum Screen { Config, Inventory, Diff, Run, Canary, Results }`.
- `crates/harness-tui/src/app.rs:182-206`:
  ```rust
  pub enum Msg { Key(KeyEvent), Tick, Controller(ControllerMsg), RunLoaded(LoadedRun), StartFailed(String), Error(String) }
  pub enum Effect { StartRun(Box<StartRequest>), CancelRun, LoadRun(PathBuf) }
  ```
- `crates/harness-tui/src/terminal.rs`: the loop polls Crossterm with `TICK = 100ms`, converts
  input to `Msg`, applies `App::update`, performs `Effect`s, redraws. `TerminalGuard` restores the
  terminal on exit, error and panic (ratatui 0.30 `try_init` panic hook). Keep this design.
- `crates/harness-cli/src/commands.rs:165-188`: `pub fn tui(config_path, output_root, err) -> i32`
  builds the old app and a `CoreController`. `crates/harness-cli/src/main.rs` defines
  `Command::Tui { config, runs_dir }`.
- Tests: `crates/harness-tui/tests/{reducer,render,model,session}.rs` and `tests/common/mod.rs`.
  `render.rs` renders into ratatui `TestBackend` and asserts on buffer text and colours. Keep
  this testing style.
- From plan 002 (read its public API in `crates/harness-agent/src/lib.rs` before starting):
  `Agent`, `AgentConfig`, `StopPolicy::Interactive`, `AgentEvent` (including
  `ApprovalRequested(ApprovalRequest, oneshot::Sender<Decision>)`), `ModelRegistry`
  (`configs/models.toml`, aliases `luna, sol, astra, flash, pro`), `Effort`, `McpServerConfig`
  (standard `mcp.json`), `ToolRegistry` with `NamingMode::Namespaced`, `DriftReport`,
  `run_adaptation`, `accept`, transcript writer.

Actual `harness-agent` API (built by plan 002, verified 2026-10-05; use these names):
- `agent::{Agent, AgentConfig, AgentParts, AgentError, StopPolicy}`. `AgentParts { models:
  ModelRegistry, clients: HashMap<ProviderId, Arc<dyn ModelClient>>, tools: ToolRegistry,
  builtins: Option<BuiltinTools>, ledger: Arc<Mutex<CostLedger>>, policy: ApprovalPolicy,
  transcript: Option<TranscriptWriter> }`. `Agent::new(config, parts)`, `run_turn(...)` (async,
  `&mut self`), `set_model(alias, effort)`, `add_mcp_connection(name, Arc<McpConnection>)`,
  `report_drift(...)`, `tools_mut()`, `ledger()`, `history()`, `reset()`. `AgentConfig` has a
  `max_output_tokens` field (default 32,000) in addition to the plan-002 fields.
- `events::{AgentEvent, StopReason}` (`StopReason` includes `Stalled`).
- `approval::{ApprovalClass, ApprovalPolicy, ApprovalRequest, Decision, PolicyAction, unified_diff}`.
- `mcp::{McpConfig (load / from_json_str), McpServerConfig, McpConnection (connect_stdio,
  connect_http, list_tools, call_tool, shutdown), LiveTool}`.
- `tools::{ToolRegistry (new(NamingMode), add_mcp_from_kb -> ExposeReport, remove_mcp_server,
  names_by_server), NamingMode, namespaced_name, builtin::BuiltinTools}`.
- `drift::{detect_drift(server, kb, live) -> (DriftReport, ToolKb), run_adaptation(...),
  AdaptationOutcome, SessionKb (new, kb, accept(&Candidate) -> usize), kb_subset, live_kb}`.
- `bridge::BlockingProvider` (call from `spawn_blocking`; drift adaptation uses it).
- `model::{openai::OpenAiClient::from_env(env, base_url), gemini::GeminiClient::from_env(env,
  base_url), mock::{MockModelClient::from_fixture_dir(provider, dir), Scripted}}`.
- `registry::{ModelRegistry::load, Effort::parse, ProviderId}`, `transcript::TranscriptWriter::create(dir, redactor)`.
- A cancelled model call is settled in the ledger as unknown usage (worst case), so `/cost`
  should show unreconciled spend separately.

Conventions: pure render functions of state (`ui::render(&App, &mut Frame)`); the reducer never
does I/O; credentials never rendered or logged (`secret.rs`). API keys come from the env vars
named in `configs/models.toml` (`OPENAI_API_KEY`, `GEMINI_API_KEY`); the TUI must not ask for
them or store them.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| All tests | `cargo test --workspace --all-features` | exit 0 |
| TUI tests | `cargo test -p harness-tui --all-features` | exit 0 |
| CLI help | `cargo run -p harness-cli -- tui --help` | exit 0, shows the new flags |

## Scope

**In scope**:
- `crates/harness-tui/**` (rewrite; delete experiment-console modules and their tests)
- `crates/harness-cli/src/main.rs` and `crates/harness-cli/src/commands.rs`: only the `tui`
  subcommand and its helpers (`build_tui_app`, `build_tui_app_with_env`)
- `crates/harness-cli/src/controller.rs` and `crates/harness-cli/tests/controller.rs`: delete
  (verified 2026-10-05: `CoreController` is used only by the old `tui` command; `harness run`
  calls `run_matrix` directly); remove the `pub use controller::CoreController` line in
  `crates/harness-cli/src/lib.rs`
- `crates/harness-cli/tests/mcpevol.rs`: remove or rewrite only the test(s) that call
  `build_tui_app_with_env` (around line 76); keep its other tests unchanged
- `crates/harness-cli/tests/cli.rs`: update only the `tui` help assertions (keep `run` and
  `validate` in `help_lists_the_three_commands`; plan 004 removes `run`)
- `crates/harness-tui/Cargo.toml`, `crates/harness-cli/Cargo.toml` (dependencies)
- `fixtures/agent/**` (create: mock scripts and a sample KB for tests)

**Out of scope**:
- `crates/harness-agent/**`: if the TUI needs an agent API that does not exist, STOP and report;
  do not add agent logic in the TUI.
- `harness run`, `harness validate` and the benchmark (plan 004).
- `crates/harness-core/**`.

## Git workflow

- Work in the current working tree on the current branch; no branches or worktrees.
- Commit only if the operator asks; no AI attribution lines. Do not push.

## Steps

### Step 1: Agent bridge (`src/bridge.rs`)

The TUI loop is synchronous; the agent is async. Create `AgentHandle`:
- spawns a dedicated OS thread running a `tokio` multi-thread runtime that owns the `Agent`,
  MCP connections and transcript writer;
- accepts `UiCommand`s over a `tokio::sync::mpsc::UnboundedSender` (`SendUser(String)`,
  `Cancel`, `SetModel{alias, effort}`, `ConnectMcp(name)`, `DisconnectMcp(name)`,
  `RunDrift{server, strategy}`, `AcceptDrift{server}` (calls `SessionKb::accept`, then re-registers the server's tools via `add_mcp_from_kb`), `RejectDrift{server}`, `Shutdown`);
- delivers `UiEvent`s to the UI over a `std::sync::mpsc::Receiver` that the loop drains each tick
  (`Agent(AgentEvent)` minus the oneshot sender, `ApprovalNeeded{id, request}`, `McpStatus{...}`,
  `DriftReady{...}`, `CanaryReady{...}`, `Fatal(String)`).
- Approval: the bridge keeps the `oneshot::Sender<Decision>` in a map keyed by request id; the UI
  answers with `UiCommand::Approve{id, decision}`.
- `ScriptedAgentHandle` for tests implementing the same trait (`AgentPort`) without a runtime.

**Verify**: `cargo test -p harness-tui bridge` → a test drives `AgentHandle` with
`MockModelClient` (plan 002) through one text reply and one approval round trip.

### Step 2: New app state and reducer (`src/app.rs`)

Replace the experiment state with:

```rust
pub struct App {
    pub transcript: Vec<Entry>,        // User, Assistant{text, streaming}, ToolCard, Notice, Error
    pub input: InputBuffer,            // multi-line; Enter sends, Shift+Enter or Alt+Enter newline
    pub status: StatusLine,            // model alias, effort, tokens in/out, USD spent / cap, MCP up/total, drift count
    pub overlay: Option<Overlay>,      // Approval(ApprovalView), Drift(DriftView), Help, Picker(PickerView)
    pub busy: bool,                    // a turn is running
    pub scroll: ScrollState,
}
pub struct ToolCard { pub call_id: String, pub name: String, pub args: String, pub output: Option<String>, pub is_error: bool, pub expanded: bool, pub duration_ms: Option<u64> }
pub enum Msg { Key(KeyEvent), Tick, Ui(UiEvent), Error(String) }
pub enum Effect { Send(UiCommand), Quit }
```

Key bindings: `Enter` send (ignored while `busy`, show a notice); `Esc` close overlay; `Ctrl-C`
cancel the running turn, or quit when idle (press twice within 1s); `PgUp/PgDn` scroll;
`Tab` toggles the focused tool card's expansion; `?` help overlay. While an approval overlay is
open, only `y` (Once), `a` (allow for session), `n`/`Esc` (Deny) act.

Slash commands parsed on send (`src/commands.rs`):

| Command | Effect |
|---|---|
| `/model` | Picker overlay of registry aliases; selecting sends `SetModel` with that model's default effort |
| `/model <alias>` | `SetModel` directly; unknown alias → error notice |
| `/effort <level>` | `SetModel` with current alias; level not in the model's `allowed_efforts` → error notice listing allowed values |
| `/mcp` | list servers with status (connected, tool count, managed by KB or not) |
| `/mcp connect <name>` / `/mcp disconnect <name>` | connect or disconnect |
| `/kb` | show KB id, version and card count per server |
| `/kb adopt <server>` | snapshot the live tools of an unmanaged server into the session KB (asks for confirmation) |
| `/drift` | open the drift overlay |
| `/cost` | notice with spent, unreconciled and remaining USD from the ledger |
| `/clear` | clear the visible transcript (the session transcript file keeps everything) |
| `/help`, `/quit` | help overlay; quit |

**Verify**: `cargo test -p harness-tui reducer` → new reducer tests cover every command row,
the busy guard, approval keys, Ctrl-C cancel vs quit, and streaming `TextDelta` appending to the
last assistant entry.

### Step 3: Rendering (`src/ui/*.rs`)

Delete `ui/{canary,config,diff,inventory,results,run}.rs` and `ui/labels.rs` if unused. Create:
- `ui/chat.rs`: transcript; user entries prefixed `> `; assistant text wrapped; a streaming
  assistant entry ends with a cursor block; tool cards one line collapsed
  (`▸ <name>(<args truncated to width>)  ✓ 120ms` or `✗`), expanded shows args pretty-printed
  and output (first 40 lines, then `… N more lines`).
- `ui/input.rs`: bordered input box, grows to 6 lines max; shows `working…` while busy.
- `ui/status.rs`: one-line status bar as listed in `StatusLine`.
- `ui/approval.rs`: centered modal: tool name, class, summary, unified diff for writes (added
  lines green, removed red), shell command verbatim, key legend.
- `ui/drift.rs`: per-server list of changes from `DriftReport.diff` (tool, field path, change
  class), then actions `[n] no update  [f] full rewrite  [s] selective patch` → sends `RunDrift`;
  when the canary report arrives show pass/fail per probe and `[a] accept  [r] reject`.
- `ui/help.rs`, `ui/picker.rs`.
Reuse `diffview.rs` for the approval and drift diffs if it fits; otherwise delete it.

**Verify**: `cargo test -p harness-tui render` → `TestBackend` tests (pattern: the existing
`tests/render.rs`) for: empty session; a streamed reply; collapsed and expanded tool cards;
approval modal with diff colours; drift overlay before and after a canary report; status bar
contents; an 80x24 terminal does not panic with long lines.

### Step 4: Terminal loop (`src/terminal.rs`)

Keep `TerminalGuard`, `TICK` and the poll loop. Each tick: drain `UiEvent`s into `Msg::Ui`,
poll keys, apply effects (`Send` → `AgentPort::send`), redraw. On quit send `Shutdown` and join
the agent thread with a 5 s timeout. Enable bracketed paste so pasted multi-line text lands in
the input box.

**Verify**: `cargo test -p harness-tui session` → a scripted session test (pattern: existing
`tests/session.rs`) runs the loop against a scripted input source and `ScriptedAgentHandle`
until quit, and asserts the terminal-restore hook ran.

### Step 5: CLI entry point

Replace `Command::Tui { config, runs_dir }` with:

```
harness tui [--workspace <dir>] [--model <alias>] [--effort <level>]
            [--models <path>] [--mcp-config <path>] [--kb <path>]
            [--sessions-dir <dir>] [--budget-usd <f64>] [--mock-script <dir>]
```

Defaults: workspace = current dir; models = `configs/models.toml`; mcp-config none (no servers);
kb none (every MCP server unmanaged until `/kb adopt`); sessions-dir
`<workspace>/.harness/sessions`; budget 5.0 USD per session; model `luna`. `--mock-script`
selects `MockModelClient` (no network) for demos and tests. Missing API key env var for the
chosen model → print `error: environment variable OPENAI_API_KEY is not set` (name only) and exit
1 before opening the terminal. Unmanaged MCP servers connect but expose no tools until adopted;
show this in `/mcp`.

**Verify**: `cargo run -p harness-cli -- tui --help` → exit 0 and lists every flag above;
`cargo test -p harness-cli` → exit 0 after updating the `tui` help test.

### Step 6: Remove dead code and run all gates

Delete `controller.rs`, `form.rs`, `inventory.rs`, `runmodel.rs` and their tests
(`tests/model.rs` if it only covers `runmodel`) once nothing references them. Keep `secret.rs`
only if still used. Update `lib.rs` docs to describe the new layout.

**Verify**: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo test --workspace --all-features` → all exit 0;
`grep -rn "Screen::Inventory\|RunController\|StartRequest" crates/harness-tui` → no matches.

### Step 7: Manual smoke (no network)

Executor note (added 2026-10-05): an agent executor cannot type into an interactive terminal. It
must run the same scenario as an automated test that drives the real terminal loop with a scripted
input source, the real `AgentHandle` and the `fixtures/agent/hello` mock script, rendering into
`TestBackend`; the interactive run below then remains a manual check for the owner.

`cargo run -p harness-cli -- tui --mock-script fixtures/agent/hello --workspace "$(mktemp -d)"`:
type `hello`, see the scripted reply stream; the script's second turn calls `write_file` →
approval modal appears; press `n` → card shows the denial; `/model sol`, `/effort none` → error
notice listing allowed efforts; `Ctrl-C` twice quits and the terminal is restored. Record what you
saw in your report. Create `fixtures/agent/hello/` with the mock responses this needs.

**Verify**: the session directory contains `transcript.jsonl` whose lines include
`"schema":"agent-session/1"`.

## Test plan

- Rewrite `tests/reducer.rs`, `tests/render.rs`, `tests/session.rs`; add `tests/bridge.rs` and
  `tests/commands.rs`. Keep `tests/common/mod.rs` as the helper home.
- Cases: every slash command (valid and invalid), effort validation per model, approval Once /
  Session / Deny, cancel mid-stream, MCP unmanaged vs managed, drift flow end to end with the
  mock client (diff → strategy → canary → accept changes the exposed tool description), long
  output truncation, small terminal sizes.
- No test may contact a provider or spawn a real MCP server except through plan 002's
  in-process test server.

## Done criteria

- [ ] Workspace fmt, clippy and test gates exit 0
- [ ] `cargo run -p harness-cli -- tui --help` lists `--workspace --model --effort --models --mcp-config --kb --sessions-dir --budget-usd --mock-script`
- [ ] `grep -rn "Screen::\(Config\|Inventory\|Diff\|Run\|Canary\|Results\)" crates/` → no matches
- [ ] `grep -rn "api_key\|API_KEY" crates/harness-tui/src | grep -v "env var name"` shows no code that reads, stores or renders a key value
- [ ] Manual smoke (Step 7) completed and described in the report
- [ ] `git status --short` shows changes only under in-scope paths
- [ ] `plans/README.md` row for 003 updated

## STOP conditions

- Plan 002 is not DONE, or `harness-agent`'s public API lacks something this plan needs
  (for example no way to answer an approval or to run drift).
- Removing the old TUI breaks `harness run` or `harness validate` (they belong to plan 004;
  leave the code they need in place and report).
- ratatui 0.30 or crossterm cannot do bracketed paste or the modal layering described; report
  the limitation instead of inventing a different UX.
- A test needs network access.

## Maintenance notes

- New agent events must be added to `UiEvent` and the reducer together; reviewers should check
  that no `AgentEvent` is silently dropped.
- The approval overlay is a safety control: review that no key path approves by default and that
  `Session` approval is per class and resets on restart.
- Deferred: session resume from a transcript, tool-output paging beyond the 40-line preview,
  editing the MCP config from inside the TUI (`mcp add` with a pre-save health check, as in
  codegraff's design), theming.
