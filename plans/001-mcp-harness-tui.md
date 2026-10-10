# Plan 001: Build the Rust MCP harness TUI and core

> **Executor instructions**: Implement the local harness described below.
> Follow the steps in order, run each verification gate before continuing,
> and stop on any listed STOP condition. Update the status row in
> `plans/README.md` when finished.
>
> **Drift check (run first)**: `git rev-parse --short HEAD` is expected to fail
> because this repository had no commit when the plan was written. Then run
> `git status --short --untracked-files=all` and
> `find . -maxdepth 3 -type f -not -path './.git/*' -not -path './plans/*' -print`.
> At planning time there were no project files outside `.git`. If project
> files now exist, inspect them and compare their purpose with this plan before
> proceeding. Stop if they establish a different architecture or data contract.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH, this builds a new Rust TUI/core and depends on a stable
  runtime contract with the separate Python benchmark workstream.
- **Depends on**: none
- **Category**: direction
- **Planned at**: no commit SHA available, unborn `main`, 2026-09-27

## Why this matters

The project studies whether changing only stale tool knowledge restores task
performance while preserving tools that still work. Your workstream owns the
Rust TUI, harness core, CLI, adaptation strategies, canary, model-provider
calls, cost ledger, and Rust-side integration with the Python benchmark runner.
The TUI must run the harness, inspect tool changes and patches, execute
canaries, and display evaluation, verification, and cost events in one terminal
application. A headless command path must use the same Rust core.

The project brief requires three strategies (no update, full rewrite, selective
patch), Early/Middle/Late server versions, Tool/Parameter/Description changes,
TF, PE, ECS, recovery and retention results, update costs, and under-update and
over-update examples. The ethics review adds request-grounded probes, a detector
for false completion claims, complete cost accounting, and a three-outcome
verifier. Per the project plan, you own the Rust + Ratatui TUI/core. The Python
workstream owner implements the benchmark adapter, completion verifier and
detector, scoring, and failure analysis. This plan assigns that Python code to
them and limits your scope to Rust and the shared interface. The latest user
clarification governs inputs and terminology: the 123-server MCP catalog and
evaluation data are supplied; a canary is a small test probe against a recent
change; the product is a TUI that is also the harness.

## Current state

The repository was empty apart from Git metadata. It had no commit, source
files, build configuration, tests, CI, project conventions, or verification
commands. There are no code excerpts to preserve. The supplied PDFs and DOCX
were read during planning:

- `Project brief.pdf`, sections 2 through 4.3: fixed-model comparison, allowed
  strategies, metrics, benchmark protocol, code/report/presentation deliverables.
- `AI Ethics Split.pdf`, “Overall Strategy” and Weeks 1 through 3: probes from
  the original request, detector labels for actually-complete versus falsely
  claimed-complete runs, granular call costs, and confirmed-success,
  confirmed-failure, or inconclusive outcomes.
- `Project plan.docx`, “What we are building”, “How it fits together”,
  “Who does what”, and “Key decisions”: Rust + Ratatui TUI/core, Python
  benchmark/analysis workstream, JSON Tool KB, JSONL run logs, OpenAI/Gemini/
  local model options, and an approximately USD 100 budget.

The supplied server catalog and evaluation files were not present in the
repository during planning. Do not recreate them or assume their on-disk format.
Locate the canonical supplied files at execution time, record release IDs,
source-relative file names, and checksums in local run configuration, and
validate their actual counts and metadata. Keep absolute local input roots in
the `MCP_DATA_DIR` and `EVAL_DATA_DIR` environment variables, not in committed
files. The benchmark brief describes a full release of 201 tasks, 123
servers, and 1,272 tools; treat those as expected full-release metadata and
confirm against the supplied release. Do not assume the supplied evaluation
subset contains all 201 tasks.

Ratatui's current quickstart uses `ratatui::init()` and `ratatui::restore()`;
its Crossterm example separates event polling from rendering, and its state
guidance recommends keeping state organized without overengineering. Follow
those patterns and test rendering with Ratatui's test backend:
[Ratatui quickstart](https://github.com/ratatui/ratatui/blob/main/README.md),
[Crossterm event loop](https://github.com/ratatui/ratatui/blob/main/examples/apps/demo/src/crossterm.rs),
[state patterns](https://github.com/ratatui/ratatui/blob/main/examples/concepts/state/README.md).
Use a current Ratatui 0.30.x release and include `Cargo.lock` in the delivered
project after resolving the workspace dependencies.

## Commands you will need

These Rust commands establish the repository's verification baseline; none
existed at planning time. Add their tool dependencies to the Rust workspace as
needed. Run them from the repository root. Python implementation and test
commands belong to the separate benchmark workstream and are outside this plan.

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Rust formatting | `cargo fmt --all -- --check` | Exit 0, no formatting diff |
| Rust lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Exit 0, no warnings |
| Rust tests | `cargo test --workspace --all-features` | Exit 0, all unit and integration tests pass |
| Config validation | `cargo run -p harness-cli -- validate --config fixtures/smoke.toml` | Exit 0 and reports the fixture's one task and local mock provider |
| Offline end-to-end smoke | `cargo run -p harness-cli -- run --config fixtures/smoke.toml --output target/harness-smoke` | Exit 0, runs all three strategies against fixtures without network or paid API calls; writes schema-valid run events and summary metrics |
| CLI contract | `cargo run -p harness-cli -- --help` | Exit 0 and lists `tui`, `validate`, and `run` commands |

The smoke configuration must use deterministic in-process fixtures and a mock
provider. Verification must not call a paid model, contact a live MCP service,
submit an evaluation job, or consume the real experiment budget.

## Suggested executor toolkit

- Read the current Ratatui quickstart and examples linked above before
  implementing the terminal lifecycle or event loop.
- Use the Ratatui references above when implementing terminal lifecycle,
  event handling, rendering, and `TestBackend` checks.

## Scope

**In scope** (create the files needed for these components):

- Rust Cargo workspace, `crates/harness-core`, `crates/harness-cli`, and
  `crates/harness-tui`.
- Rust-side benchmark runner interface and fixture JSON/JSONL contract tests.
  The Python benchmark project itself is owned by the other workstream.
- Versioned `schemas/tool-kb.schema.json` and
  `schemas/run-event.schema.json` for the only Rust/Python runtime interchange
  files: Tool KB JSON and run-event JSONL.
- `configs/` for reproducible experiment settings, `fixtures/` for local
  smoke-only MCP/evaluation inputs, and `docs/experiment-protocol.md` for the
  fixed task subset and model/judge settings.
- Root `README.md`, `CONTRIBUTING.md`, `.gitignore`, and
  `.github/workflows/ci.yml` for the Rust setup, commands, generated-output
  handling, and Rust verification gates in this plan.
- Runtime output under `runs/<run-id>/`, containing the TOML manifest and raw
  JSONL event/action traces needed to reproduce and analyze a run. Never put
  provider keys or credentials in these artifacts.

**Out of scope**:

- Editing, vendoring, relabeling, or redistributing the supplied MCP catalog,
  evaluation tasks, benchmark judge, or benchmark source. Treat these inputs as
  immutable and refer to local paths or installed benchmark packages.
- Fine-tuning any model, changing benchmark labels, or changing base-model,
  decoding, task-set, or judge settings between strategies.
- Creating or modifying the Python benchmark adapter, completion verifier or
  detector, scoring and failure-analysis scripts, Python tests, or Python
  dependency/configuration files. Person B owns that workstream. This plan
  covers the shared contract and Rust-side consumption only.
- Calling real paid providers, running the full experiment matrix, deploying,
  pushing, submitting jobs, or writing the final report/presentation while
  implementing and verifying this plan. The finished harness must support
  these runs; the project owner starts them separately.
- Anthropic provider support. The project plan specifies OpenAI, Gemini, and
  local providers.

## Git workflow

- Work directly on the current `main` branch in this repository. Do not create
  a worktree or branch.
- There is no observed commit-message convention because no commit exists.
  Do not commit or push as part of plan execution. Do not deploy or submit
  external jobs.

## Implementation architecture and contracts

Keep the Rust domain pipeline independent from the TUI. The TUI and headless
CLI must call the same `harness-core` services. Person B owns the MCPEvol-Bench
adapter, completion verifier/detector, score aggregation, and failure analysis.
Your Rust implementation validates and consumes their agreed output; it must
not reimplement their benchmark logic. The Rust/Python file boundary consists
only of:

1. **Tool KB JSON**: schema-versioned tool cards with server ID, source-native
   tool ID/name, server version, description, input/output schemas, and
   applicable usage rules. Preserve source provenance for every card. Use the
   benchmark's stable lineage ID for a tool across versions when available;
   otherwise record an explicit mapping from the supplied change metadata so
   renames are not mistaken for unrelated removals and additions.
2. **Run-event JSONL**: schema-versioned, typed events for configuration,
   adaptation diffs, model-call usage, MCP actions, canary results, verifier
   probes/verdicts, metric scores, errors, and completion. Each line is one
   independently valid JSON object. Do not write a separate Rust/Python
   interchange format.

Keep Rust-side experiment configuration in TOML. It must pin the task IDs,
Early/Middle/Late source versions, strategy, model/provider and decoding
settings, judge version, repetition/seed, output path, and budget. In Step 1,
agree with Person B on a runner invocation that passes these values as explicit
argv fields and paths, plus the remaining budget. Do not introduce a third
shared file format. Spawn the runner with argument arrays, never a
shell-interpolated command string, and never pass secrets as argv values.

Suggested responsibilities:

- `harness-core`: input validation, normalized tool cards, version diffing,
  three strategy implementations, provider interface, patch validation,
  canary gate, call-cost ledger, event emission, and run orchestration.
- `harness-cli`: `tui`, `validate`, and headless `run` commands over the same
  core services.
- `harness-tui`: Ratatui state, event handling, and rendering. Keep state and
  rendering separate. Restore the terminal after normal exit, returned errors,
  and panics.
- Rust/Python bridge: a Rust `BenchmarkRunner` interface, production process
  launcher using the agreed argv contract, JSONL parser, and UI event model.
  Tests use a fake runner and checked-in contract fixtures. Python changes are
  done by Person B and are out of this plan's file scope.

## Steps

### Step 1: Locate and freeze supplied inputs and evaluation protocol

Find the canonical supplied MCP server catalog and evaluation data. Do not use
server discovery to replace the catalog. Read the input roots from
`MCP_DATA_DIR` and `EVAL_DATA_DIR`; record release/version IDs, source-relative
file names and SHA-256 checksums, server and tool counts, evaluation task IDs,
server version coverage, and change-type coverage in
`docs/experiment-protocol.md`. Build a fixed pilot task set that covers Tool-,
Parameter-, and Description-level changes across Early, Middle, and Late
versions. Use the brief's 20 to 30 task subset as the planned initial no-update
baseline. The implementation executor must not run a real provider pilot; the
project owner starts that run after the harness is ready. After the owner
reviews pilot cost, freeze the common task set and settings used by all
strategies.
Document whether the final protocol uses three repetitions or one repetition
because of the budget. Keep the same base model, decoding, task set, and judge
settings for each strategy.

Define and version the two JSON interchange schemas before the Rust and Python
workstreams split. Agree the Python runner's executable and argv contract with
Person B. It accepts the Tool KB path, selected task IDs and server version,
fixed model/judge settings, output JSONL path, and remaining budget. It emits
task/action, completion-verifier, metric, failure-category, and per-call cost
events using the agreed JSONL schema. Record the exact command, required
flags, exit behavior, and event types in `docs/experiment-protocol.md`. Person
B implements the Python runner and events; you implement only the Rust
launcher, parser, and UI consumption. If the contract is not agreed, keep the
Rust runner behind a trait and use fixtures until agreement arrives.

Write representative Tool KB and event fixtures for each agreed event type.
Keep the original supplied inputs read-only and out of Git unless their owner
has explicitly approved redistribution.

**Verify**: Before creating project files,
`git status --short --untracked-files=all` shows only plan files.
`test -d "$MCP_DATA_DIR" && test -d "$EVAL_DATA_DIR"` exits 0. After writing
the protocol, inspect it and confirm it names the source-relative files and
checksums, actual counts, frozen task IDs, all three change classes, three
version stages, model settings, judge settings, and repetition choice. If
either input root is unset or the supplied files cannot be located, stop and
report the missing path.

### Step 2: Scaffold the workspace and local verification baseline

Create the Rust workspace and three crates named above. Pin the available Rust
toolchain and create `Cargo.lock`. Add a deterministic Rust smoke fixture with
one MCP server, one evaluation task, and a mock model provider. Add Rust
validation for the Tool KB and run-event schemas. Configure CI to run the Rust
formatting, lint, and test commands in the table above. Do not create Python
project files or add Python dependencies.

Create Rust tests that load the shared contract fixtures and reject a
deliberately malformed tool card or event. Do not add networked tests.

**Verify**: Run the Rust formatting, Clippy, and test commands from the command
table. All must exit 0. Contract fixtures must fail validation when required
schema fields are removed and pass when intact. `git status --short` must show
no changes under `benchmark/`.

### Step 3: Implement tool diffing and the three adaptation strategies

In `harness-core`, ingest the supplied server-version metadata through a
benchmark-specific adapter and normalize it into stable tool cards. Compare
the selected source and target versions and classify changes at tool,
parameter/schema, and description/usage-rule level. Preserve rename/removal
information and output-schema changes when the source release exposes them.
Never infer a changed field from the task text alone.

Implement these strategy invariants:

- **No update** returns the original Tool KB unchanged.
- **Full rewrite** regenerates the complete selected Tool KB from the target
  version.
- **Selective patch** updates only cards and fields associated with detected
  changes. Record changed IDs, field paths, old/new hashes, and source version.
  Serialize cards deterministically and assert that each unaffected card is
  unchanged byte-for-byte.

Validate every candidate against the JSON schema and target MCP definitions
before it can be promoted. Preserve the original Tool KB for rollback. Add a
provider interface for OpenAI, Gemini, and local OpenAI-compatible endpoints.
Keep provider/model and decoding settings fixed for a comparison run. Accept
only schema-valid structured patch output; reject invalid or incomplete
responses without silently broadening the patch.

**Verify**: `cargo test --workspace --all-features` includes cases for no
change, tool add/remove/rename, parameter add/remove/type/required changes,
description changes, output-schema changes, full rewrite coverage, selective
patch boundaries, and no-update identity. Tests prove that an unrelated tool
card remains unchanged after each relevant selective patch case. Run the
fixture config validation command and confirm it prints the expected strategy
set and schema versions.

### Step 4: Add canary gating and complete cost accounting

Define a canary separately from the completion verifier. After a candidate
patch, issue one to three small, targeted test probes against the changed
tool(s) in the target server version. Use isolated/resettable benchmark state.
When an unchanged control task is available, include one control probe to
detect collateral damage. Record pass, fail, or inconclusive with the tool ID,
probe/task ID, expected observation, actual observation, and event references.
Promote a candidate only after a passing canary. On failure or inconclusive
result, preserve the prior KB and mark the candidate blocked; do not represent
it as a successful update. Do not issue a mutating probe against
non-disposable state. No-update runs have no candidate to probe; record their
canary status as `not_applicable`.

Emit one Rust cost event for every paid or local-model request made by
adaptation and canary. The Python workstream owner must emit the same fields
for verifier/probe-generation calls and other Python model requests. Include
provider, model, phase, request ID, input/output tokens when returned, latency,
configured rate-card ID/date, estimated USD cost, and cost status. Do not log
API keys. Unknown provider usage or price is `unknown`, never zero. Validate
and aggregate both workstreams' cost events in the Rust run view.
Configure the approximately USD 100 local dispatch cap in TOML. Before each
paid call, set a maximum output-token limit, reserve a conservative cost using
the configured rate card and request token count, and refuse dispatch if that
would exceed the remaining cap. Reconcile the reservation against returned
usage and the versioned rate card. Block paid calls when no rate is configured.
Record local inference time separately from paid API cost. Label this as a
local dispatch cap in the UI; do not claim it guarantees the provider's final
bill. Before launching a Python run stage, pass the remaining budget through
the agreed argv contract. Person B's runner enforces that stage limit and
returns per-call cost events; Rust checks aggregate cost before starting the
next stage.

**Verify**: Rust tests cover passing, failing, and inconclusive canaries;
rollback/no-promotion; unchanged control detection; all cost phases; missing
usage/pricing; budget reservation and release; refusal at the cap; API-key
redaction; and aggregation of fixture Python cost events. The offline smoke run
makes zero network/model requests and records fixture costs as mock usage, not
real USD spend.

### Step 5: Integrate the Rust core with Person B's benchmark runner

Define a Rust `BenchmarkRunner` trait and a production process-backed
implementation. The process-backed implementation launches Person B's agreed
Python runner using `std::process::Command` with an executable and argument
array, never a shell command. Pass the Tool KB JSON path, run ID, selected task
IDs, server version, fixed model and judge settings, output JSONL path, and
remaining budget as explicit arguments. Read dataset roots and credentials
from environment variables. The runner contract and exact command must be
recorded in `docs/experiment-protocol.md`; this plan does not create or modify
the Python package.

Use the versioned JSONL event schema to parse benchmark actions, completion
verdicts, metric results, failure categories, per-call usage/cost, errors, and
run completion. Reject malformed or unsupported schema versions with a clear
error that names the event line. Preserve raw events in run output. Do not
parse human-readable Python console output or reimplement its task execution,
completion verification, judging, scoring, or failure analysis.

Implement a fixture-backed runner for Rust tests. Cover valid and invalid
events, the three completion outcomes, TF/PE/ECS result events, both failure
categories, cost aggregation, nonzero child exit, and partial event logs.
When Person B's runner is available, add a non-paid integration check that
uses its mock mode. If it is not available, keep the production launch contract
and fixture tests, and report live integration as BLOCKED instead of claiming
the complete harness is integrated.

**Verify**: `cargo test --workspace --all-features` proves fixture event logs
parse and malformed events fail closed. The subprocess test confirms arguments
are passed without a shell and secrets are absent from argv and recorded logs.
If Person B's mock runner exists, the contract integration check exits 0 and
produces schema-valid JSONL. Do not implement or test Python internals in this
workstream.

### Step 6: Build the TUI and headless harness entry points

Implement the CLI contract:

- `harness tui [--config <path>]` opens the interactive application.
- `harness validate --config <path>` validates inputs and configuration
  without contacting providers or live MCP servers.
- `harness run --config <path> --output <directory>` runs the configured
  strategy/version/task/repetition matrix headlessly.

All three commands must use the same `harness-core` pipeline. The TUI must
support these operator tasks:

- Configure provider/model and decoding settings, enter credentials through
  masked input or read them from environment variables, and never save or log
  secret values.
- Inspect the supplied server/evaluation inventory and select a fixed task
  subset with visible coverage of version stages and all three change types.
- Inspect old/new tool cards and a parameter/description-level diff; review a
  candidate patch without changing benchmark ground truth.
- Start the three strategies, show progress and errors, run and inspect the
  canary gate, and prevent promotion after a failed or inconclusive canary.
- View task completion verdicts, TF/PE/ECS, recovery/retention, failure
  trajectories, token/time/API-cost totals, per-phase costs, and budget
  remaining.
- Save a reproducible local run and reopen its event log for inspection.

Treat patch inspection as read-only during official comparison runs. A manual
edit creates a distinct, explicitly named experimental variant with its own
run record; never fold hand edits into the standard selective-patch result.
Keep run state in application types, handle terminal events separately from
rendering, use Crossterm polling, and restore terminal state on normal exit,
errors, and panic.

**Verify**: `cargo test --workspace --all-features` includes Ratatui `TestBackend`
render tests for empty inventory, diff, failed canary, inconclusive verifier,
and results screens, plus event/state tests for navigation, selection, run
start/cancel, and error display. Run CLI help, config validation, and offline
end-to-end smoke commands from the table. The smoke run must write all three
strategies and events for adaptation, canary or `not_applicable`, verification,
and metrics under `target/harness-smoke` with no network access.

### Step 7: Document and freeze a reproducible pilot

Write setup instructions for Rust, supplied data paths, provider configuration,
CLI/TUI commands, schemas, output layout, and the external Python runner
contract. Document fixed pilot task IDs, server versions, model/inference
settings, judge version, repetitions, rate-card date, and excluded tasks in
`docs/experiment-protocol.md`. Preserve raw JSONL files from both workstreams
so their results can be inspected and recomputed by Person B's analysis tools.
Add generated run outputs and local credentials to `.gitignore` only as
appropriate; do not ignore raw result artifacts intended for the project
deliverable. Sanitize secrets at event creation and test that sanitation.

**Verify**: Run every Rust verification command in the table. Run the local
mock smoke config once and inspect its output directory: it contains the Rust
manifest, schema-valid JSONL events, all three adaptation outcomes, canary
status or `not_applicable`, and mock cost records. If Person B's mock runner is
available, also run the contract integration check and inspect its completion,
metric, failure, and cost events. `git status --short --untracked-files=all`
contains only intended Rust project and contract files; no Python
implementation, source dataset, API key, or provider secret was added.

## Test plan

- Rust core unit tests: version diff classification, strategy invariants,
  schema rejection, deterministic event writing, cost tracking, and canary
  promotion/rollback. Put beside modules under `crates/harness-core/src/` or
  in `crates/harness-core/tests/`.
- Rust bridge contract tests: one Tool KB JSON file and representative
  Python-produced JSONL event lines under `fixtures/contracts/`. Rust validates
  the fixtures against the checked-in schemas and covers malformed, missing,
  and unsupported events without implementing Python behavior.
- TUI tests: Ratatui `TestBackend` for rendering and pure event/state tests.
  Do not use a simulator, live terminal automation, external model, or network
  service in automated verification.
- End-to-end: one deterministic local Rust smoke task, mock provider, all three
  strategies, canary or `not_applicable`, and run artifact output. No paid
  calls. If Person B's mock runner is available, run one additional contract
  smoke that consumes its JSONL output.

## Done criteria

- [ ] `docs/experiment-protocol.md` identifies the supplied input checksums,
  actual catalog/evaluation counts, fixed task IDs, version stages, change
  classes, fixed model/judge configuration, and repetition count.
- [ ] TUI and headless CLI execute the same core pipeline for no-update,
  full-rewrite, and selective-patch strategies.
- [ ] The full supplied evaluation can be selected; the documented pilot
  covers Early/Middle/Late and Tool/Parameter/Description changes.
- [ ] Canary is a small probe against the recent patch, distinct from the
  request-grounded completion verifier; it is `not_applicable` for no-update,
  and failed/inconclusive canaries do not promote candidate knowledge.
- [ ] The Rust bridge validates and displays Person B's completion-verifier
  events, including confirmed success, confirmed failure, and inconclusive.
- [ ] The Rust UI displays and preserves Person B's TF, PE, ECS,
  recovery/retention, and under/over-update events without reimplementing their
  scoring or analysis.
- [ ] Every Rust adaptation/canary call is cost logged, Python per-call cost
  events are validated and included in totals, and the remaining budget is
  passed to the Python runner before its stage starts.
- [ ] All verification commands in the command table exit 0; the offline smoke
  test makes no external request.
- [ ] No supplied benchmark labels, judge, or server definitions were modified;
  no secrets or original source datasets are committed.
- [ ] No Python implementation, dependency, or test files were created or
  modified. The live runner integration is verified, or its unavailable status
  is reported as BLOCKED.
- [ ] No files outside the implementation scope are changed; the executor
  updates the status row in `plans/README.md`.

## STOP conditions

- The supplied catalog or evaluation data cannot be located or their formats
  cannot be read. Ask for the canonical path; do not substitute fabricated or
  remotely discovered server/evaluation data.
- Supplied metadata conflicts with 123 servers or the evaluator/task/version
  contract. Record the discrepancy and stop before freezing a task subset.
- The benchmark adapter requires editing labels, judge semantics, or upstream
  benchmark code. Stop and report the minimum required interface change.
- Canary probes cannot run against isolated/resettable benchmark state, or a
  probe could mutate a non-disposable service. Stop before running that probe.
- A paid provider lacks configured pricing or the required usage metadata to
  enforce/log the local budget. Do not send paid calls until resolved.
- Person B's runner command, budget behavior, or JSONL event contract is not
  agreed. Continue independent Rust work with fixtures, but mark the live
  benchmark integration BLOCKED and do not claim the harness is end-to-end.
- Shared Tool KB or JSONL schema changes after Rust/Python work begins. Stop,
  version the contract, and coordinate both sides before continuing.
- Existing project files have appeared and contradict the empty-repository
  assumptions in this plan.

## Maintenance notes

- Keep the Tool KB and run-event schemas versioned. A field change requires
  updating both language validators, shared fixtures, and contract tests.
- Benchmark release, task selection, MCP server versions, model identifier,
  decoding, judge, and price-card date are experiment inputs. Record them per
  run; never compare results whose settings drifted without labeling the change.
- Review selective-patch boundaries and serialized hashes whenever tool-card
  normalization changes. Otherwise “unaffected knowledge remained untouched”
  can become unprovable.
- Keep canary records separate from completion verdicts. A canary tests the
  recent adaptation; the verifier checks an agent's completion claim against
  the original request and benchmark state.
- The TUI is a consumer of Person B's completion and metric events. When those
  semantics change, update the shared event contract and UI presentation in
  coordination; keep probe generation and score calculations in the Python
  workstream.
- Reconcile usage from provider responses with the local cost ledger. Unknown
  usage or pricing must remain visible as unknown instead of being reported as
  free.
