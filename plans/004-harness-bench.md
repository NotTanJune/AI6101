# Plan 004: Add `harness bench` (MCPEvol tasks through the shared agent loop) and retire `harness run`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update the status row for this plan in
> `plans/README.md`, unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: confirm plans 002 and 003 are DONE in
> `plans/README.md`. Confirm the benchmark checkout exists at the path in
> `MCPEVOL_ROOT` (default `~/Code-Projects/MCPEvol-Bench`) and that
> `git -C "$MCPEVOL_ROOT" rev-parse HEAD` prints
> `888c779a0fe735f612ca62bdd1fdd7c9e6bf8701`. Run `git status --short` and,
> if commits exist, `git diff --stat <first commit>..HEAD -- crates/harness-cli crates/harness-core/src/mcpevol.rs .github docs`.
> Compare excerpts below with live files. On any mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH (paid API calls; external benchmark contract; live MCP servers)
- **Depends on**: plans/002-harness-agent-core.md, plans/003-coding-agent-tui.md
- **Category**: direction
- **Planned at**: no commit SHA (unborn `main`, all files untracked), 2026-10-04

## Why this matters

The project measures whether updating only stale tool knowledge (the Tool KB) restores task
performance on MCPEvol-Bench while keeping tools that still work. The owner decided on
2026-10-04 to benchmark our own Rust agent: the same agent loop as the TUI, with built-in tools
off, run against live MCP servers through 1mcp, writing answer files that Person B's Python side
judges with the benchmark's own scripts. `harness run` (plan 001) replays fixtures through a
Python-runner contract that is now abandoned. This plan adds `harness bench`, a pilot mode that
measures real cost before any full run (the budget target is about USD 100 and a rough estimate
for all five models is about USD 409), and removes `harness run`.

## Current state

### This repository

- `crates/harness-cli/src/main.rs`: subcommands `Tui` (rewritten by plan 003), `Validate`,
  `Run`. `crates/harness-cli/src/commands.rs:81-119` implements `run`; exit codes at
  commands.rs:23-31 (`EXIT_OK=0, EXIT_ERROR=1, EXIT_FAILED=3, EXIT_CANCELLED=4, EXIT_BUDGET_EXHAUSTED=5`).
- `.github/workflows/*.yml` runs: fmt, clippy, test, `harness --help`,
  `harness validate --config fixtures/smoke.toml`,
  `harness run --config fixtures/smoke.toml --output target/harness-smoke`.
- `crates/harness-core/src/mcpevol.rs`: `McpEvolAdapter` (implements
  `source::SourceAdapter`: `load_kb(version)`, `version_labels()`), `evolution_file(depth)`
  (`mutation/HYBRID/.../evolution.json`, one `HYBRID` per depth), `Mutation::involves`,
  `task_footprints()`, `stage_depths() -> [usize; 3]`. Config `[versions.stages]` maps
  Early/Middle/Late to labels; the owner's mapping is Early=`original` (depth 0),
  Middle=`iter2` (depth 2), Late=`iter4` (depth 4).
- `crates/harness-core/src/pipeline.rs`: plan 001's adaptation + canary + promotion sequence.
- From plan 002 (`crates/harness-agent`): `Agent`, `AgentConfig`, `StopPolicy::Benchmark{finish_marker, nudge_text, max_no_tool_turns}`,
  `tool_result_max_chars`, `ModelRegistry` (aliases `luna, sol, astra, flash, pro`),
  `McpConnection::connect_http(url)`, `ToolRegistry` with `NamingMode::Bare`, `drift::run_adaptation`,
  `BlockingProvider`, the extended cost ledger, `MockModelClient`.
- `plans/model-api-facts-2026-10-04.md`: model IDs, efforts and prices.

### The benchmark (`$MCPEVOL_ROOT`, commit 888c779; read these files yourself before Step 3)

Its runner is `src/custom/real_mcp/tool_level_test.py`; its agent is
`src/custom/agents/SimpleOpenAIAgent.py` (`ChatModel`); its judge is
`src/custom/analysis/answer/evaluation.py`. Facts verified on 2026-10-04:

1. **Tasks**: `src/custom/data/test_cases/test_cases.json` is a list of 393 cases with keys
   `response, question, tool_calls, servers, answer, trajectory_messages, stop, exceeded, idx, eval_result`.
   `servers` maps server name → list of tool names the task may use (3.97 tools on average, 9
   max). `chosen_idx.json` is `{"samples": [{"idx": N}, ...]}` with 201 entries.
2. **Tools shown to the model** (`return_involved_tool_status`, tool_level_test.py:76-91): only
   the tools listed in the task's `servers`, as `{name, description, inputSchema}`. Tool names
   are bare (`reverse_mapping(servers)` maps tool name → server). For evolved stages
   (`--step 4`), `change_test_case_servers` (tool_level_test.py:37-74) first rewrites the task's
   `servers` per TOOL mutations, and `return_involved_mutated_tool_status`
   (tool_level_test.py:109-140) overlays DESC mutations from `evolution.json` onto the live
   descriptions. DESC changes exist only client-side, never in the live servers.
3. **Evolved stages**: `get_all_evolution_status(mutation_path)` reads `evolution.json` at every
   `HYBRID` level of the path, so a path with N `HYBRID` segments means cumulative depth N.
   `return_involved_mutated_test_cases` (tool_level_test.py:158-170, using `_is_server_involved`
   at 142-156) keeps only tasks touched by those evolutions. Non-involved tasks reuse the
   baseline result (`count_mutation_scores` in evaluation.py).
4. **Rounds**: baseline `max_round = 10 if 2*len(involved_tools) < 10 else 2*len(involved_tools)`;
   evolved `max_round = 10 if 2*len(test_case['tool_calls']) < 10 else 2*len(test_case['tool_calls'])`.
   Loop: `while not stop_flag and round_num <= max_round`; `exceeded = round_num >= max_round`.
5. **Agent behaviour** (`ChatModel.run_with_server`, SimpleOpenAIAgent.py:200-290): messages
   start with a system prompt (SimpleOpenAIAgent.py:10-23, placeholders `{SET_TIME}` and
   `{FILE_EXISTS}`) and the user question. A reply containing `[FINISH]` stops (`stop=True`). A
   reply with text and no tool calls appends the user message
   `Please call the tool directly to complete the task without asking me any questions.`; the
   third such reply stops. A reply with neither stops (loop break). Each tool result is
   `str(result.content)[:500]+'...'`; errors become `Error: <message>`; timeouts become
   `Tool call timed out.`. `answer` = the stopping texts joined by `\n`. Exceptions append
   `{"role":"assistant","content":"<error>"}`.
6. **Tool calls go to 1mcp** over streamable HTTP (default `http://127.0.0.1:3050/mcp`) as
   `<server>_1mcp_<tool>`. 1mcp is started with
   `1mcp --port 3050 --log-level error --config servers/1mcp_config/<stage>/mcp.json`.
   `servers/1mcp_config/{original,iter3,iter5}/mcp.json` each list 167 servers; `iter3`/`iter5`
   point at Hugging Face bundles (`/path/to/iter3-servers`, `/path/to/iter5-servers`
   placeholders). Some servers need MySQL as configured in the system prompt.
7. **Answer file contract** (evaluation.py and tool_level_test.py:604-613): a flat directory
   whose path contains `ANSWER`, one `<idx>.json` per task, each a JSON object with at least
   `question`, `servers`, `tool_calls`, `idx`, `answer`, `trajectory_messages`, `stop`,
   `exceeded`, `involved_tools`. `trajectory_messages` are OpenAI Chat Completions messages:
   `{"role":"system","content"}`, `{"role":"user","content"}`,
   `{"role":"assistant","content"?, "tool_calls"?: [{"id","type":"function","function":{"name","arguments":"<json string>"}}]}`
   (no `content` key when it is null), `{"role":"tool","tool_call_id","content"}`. The judge
   drops the first two messages (`[2:]`), blanks assistant text, truncates tool content to 1000
   chars, and writes results to the same path with `ANSWER` replaced by `EVAL` (every
   occurrence).
8. **Gaps on Person B's side (report, do not fix)**: `evaluation.py` reads
   `custom/data/test_cases/real.json`, which is not in the repository (`chosen_idx.json` has the
   same shape); and the judge loads tool descriptions from a live 1mcp at
   `http://127.0.0.1:3050/mcp` at import time.
9. **Unconfirmed stage identity**: whether the `iter3`/`iter5` server bundles equal evolution
   depth 2/depth 4 is not known. All four depths mutate the same 89 servers, so server sets
   cannot tell them apart. Step 6 adds a check that compares live `tools/list` with the
   replayed KB before any paid run.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| All tests | `cargo test --workspace --all-features` | exit 0 |
| Bench help | `cargo run -p harness-cli -- bench --help` | exit 0 |
| Offline smoke | `cargo run -p harness-cli -- bench --config fixtures/bench-smoke.toml --output target/bench-smoke` | exit 0, writes ANSWER files |

## Scope

**In scope**:
- `crates/harness-bench/**` (create: library crate holding the bench logic)
- `crates/harness-cli/src/{main.rs,commands.rs,lib.rs}`, `crates/harness-cli/tests/**`,
  `crates/harness-cli/Cargo.toml`: add `bench`, remove `run`
- `crates/harness-cli/src/controller.rs`: delete if only `run` used it
- `Cargo.toml` (workspace member + dependency entry)
- `configs/bench.example.toml`, `configs/bench/system_prompt.txt` (create)
- `fixtures/bench-smoke/**`, `fixtures/bench-smoke.toml` (create)
- `.github/workflows/*.yml`: replace the `harness run` smoke step
- `docs/experiment-protocol.md`: replace the Python runner argv contract section
- `README.md`: update the command list

**Out of scope**:
- `crates/harness-agent/**` and `crates/harness-tui/**`: if bench needs an agent change, STOP.
- `crates/harness-core/src/{strategy,canary,diff,patch,mcpevol}.rs`: use their APIs only.
- Anything in `$MCPEVOL_ROOT` (read-only; never edit, never run its scripts).
- Downloading the Hugging Face `evol-servers` bundles, installing `1mcp`, `npx`/`uvx` packages,
  MySQL or containers. The operator does these separately after asking the owner.
- Running any paid model call. All tests and the smoke use `MockModelClient` and fixture MCP
  results. Pilot and full runs are executed later by the owner.

## Git workflow

- Current branch, working tree in place; no branches or worktrees.
- Commit only if the operator asks; no AI attribution lines. Do not push.

## Design (implement exactly this)

Experiment matrix: model × stage × condition × task.

- **Stages** (config `[bench.stages.<name>]`): `early` (KB label `original`, depth 0,
  `mcp_url` for the `original` 1mcp), `middle` (KB label `iter2`, depth 2, `mcp_url` for the
  `iter3` 1mcp), `late` (KB label `iter4`, depth 4, `mcp_url` for the `iter5` 1mcp). Each stage
  also records `verified = false` until Step 6's check passes.
- **Conditions** (which KB the model sees at a stage): `no_update` (Early KB unchanged),
  `full_rewrite` and `selective_patch` (Early KB adapted toward the stage KB by the core
  strategies through `BlockingProvider`, gated by the canary exactly as plan 001's pipeline
  does), and `oracle` (the stage KB itself, which equals what the benchmark's own runner shows).
  At `early` only one condition runs (all conditions share the Early KB); label it `no_update`.
  The adaptation target is the replayed stage KB from `McpEvolAdapter::load_kb(label)`, because
  DESC mutations exist only there.
- **Tasks**: `early` runs the selected task set (default: the 201 in `chosen_idx.json`).
  `middle`/`late` run only tasks involved in the cumulative evolutions up to that depth (port
  `return_involved_mutated_test_cases` and `_is_server_involved`), with the task's `servers`
  rewritten by a port of `change_test_case_servers`.
- **Tools per task**: for each `(server, tool)` in the (rewritten) task `servers`, expose the
  card from the condition's KB for that server whose lineage matches; tool names are the KB card
  names (`NamingMode::Bare`). A listed tool with no card in the condition KB is not exposed (a
  stale KB does not know it). Calls route to `<server>_1mcp_<name>` on the stage's `mcp_url`.
- **Agent settings**: built-ins off; `StopPolicy::Benchmark { finish_marker: "[FINISH]", nudge_text: <item 5 text>, max_no_tool_turns: 3 }`;
  `tool_result_max_chars = 500` then append `...`; `max_steps` from item 4 (baseline rule at
  early, evolved rule at middle/late); model effort from the registry default unless
  `--effort` overrides (per model: `luna=max, sol=xhigh, astra=high, flash=medium, pro=high`).
- **System prompt**: `configs/bench/system_prompt.txt` holds the benchmark template copied
  verbatim from SimpleOpenAIAgent.py:10-23 (it contains the benchmark's local test-database
  settings; they are fixture values from the public benchmark, keep them verbatim and do not
  treat them as project secrets). `{SET_TIME}` = local date as `YYYY-MM-DD (<weekday>)` with the
  same Chinese weekday names as `offset_date` in `src/custom/utils/test_utils.py:615-626`;
  `{FILE_EXISTS}` = a port of `generate_tree_string` (test_utils.py:555-613) run on the
  configured `annotation_dir` (default `$MCPEVOL_ROOT/anotation_path`).
- **Output layout**: `<output>/<run_id>/<model_alias>/<stage>/<condition>/ANSWER/<idx>.json`
  plus `<output>/<run_id>/manifest.json` (config, git state, model registry hash, KB hashes per
  stage/condition, stage verification results), `events.jsonl` (agent events, redacted),
  `cost.json` (ledger records). Validate that no path segment other than the final `ANSWER`
  contains the substring `ANSWER`.
- **Answer file**: the benchmark fields from item 7 (`question`, `servers` (rewritten),
  `tool_calls` (from the test case), `idx`, `answer`, `trajectory_messages`, `stop`,
  `exceeded`, `involved_tools` = the exact `{name, description, inputSchema}` list the model
  saw) plus `harness_meta`: `{run_id, model_alias, model_id, effort, stage, condition, kb_hash,
  usage, cost_usd, stop_reason, reused_from: null}`.
- **Full-stage materialisation** (`--materialize-full`, default on): after middle/late finish,
  copy each non-involved task's early answer into the stage directory with
  `harness_meta.reused_from = "early"`, so ECS can read three complete directories.
- **Budget**: one ledger per run with `--budget-usd` (required for paid models; no default).
  Every model call reserves first; refusal stops the run cleanly with exit 5 and a resumable
  state (already-written answers are skipped on rerun with the same `--run-id`).
- **Pilot**: `--pilot N` runs N tasks (seeded sample from the early set, `--seed`, default 0)
  for every selected model at every selected stage/condition, then writes
  `pilot-report.json` and prints, per model: mean and max cost per task, mean input/output/
  reasoning tokens, and the projected full-run cost = mean cost per task × (early task count +
  involved task count at middle × conditions + involved task count at late × conditions).
- **Dry run**: `--dry-run` prints the matrix (task counts per stage and condition) and the
  worst-case reservation total, makes no model or MCP call.

## Steps

### Step 1: Crate skeleton and config

Create `crates/harness-bench` (library; depends on `harness-core`, `harness-agent`, `serde`,
`serde_json`, `toml`, `tokio`, `chrono`, `thiserror`). `BenchConfig` from TOML:

```toml
[bench]
benchmark_root = "${MCPEVOL_ROOT}"      # env expansion; default ~/Code-Projects/MCPEvol-Bench
annotation_dir = ""                     # empty = <benchmark_root>/anotation_path
models_file = "configs/models.toml"
system_prompt_file = "configs/bench/system_prompt.txt"
tasks = "chosen"                        # "chosen" | [0, 3, 4] | "sample:20"
conditions = ["no_update", "full_rewrite", "selective_patch"]   # "oracle" optional
tool_call_timeout_secs = 120

[bench.stages.early]
kb_label = "original"
mcp_url = "http://127.0.0.1:3050/mcp"
[bench.stages.middle]
kb_label = "iter2"
mcp_url = "http://127.0.0.1:3051/mcp"
[bench.stages.late]
kb_label = "iter4"
mcp_url = "http://127.0.0.1:3052/mcp"
```

Validation: stages exist; kb labels are known to `McpEvolAdapter`; URLs are loopback HTTP or
HTTPS; conditions known; models exist in the registry. Write `configs/bench.example.toml`.

**Verify**: `cargo test -p harness-bench config` → valid example parses; unknown condition,
unknown model and non-loopback `http://` URL fail with clear messages.

### Step 2: Task selection, involvement and tool exposure

Port, with the benchmark source open beside you, `_is_server_involved`,
`return_involved_mutated_test_cases` and `change_test_case_servers` into
`harness-bench/src/tasks.rs`, reading `evolution.json` files via
`harness_core::mcpevol::evolution_file(depth)` for depths 1..=d. Then implement tool exposure
from a condition KB as in the Design section.

**Verify**: `cargo test -p harness-bench tasks` against the real checkout (skip with a printed
reason when `MCPEVOL_ROOT` is missing): chosen set has 201 tasks; involved counts at depth 2
and 4 are non-zero and depth 4 ≥ depth 2; for three hand-picked tasks, the rewritten `servers`
equals what you compute by hand from the Python logic (write the expected values into the test
with a comment showing the derivation). Also record the involved counts in your report.

### Step 3: Prompt, trajectory export and answer writer

- `prompt.rs`: render the system prompt as specified; unit-test `generate_tree_string` against
  a small temp directory with expected output derived by reading the Python function.
- `trajectory.rs`: convert the agent's `Item` history into Chat Completions messages per item 7
  (system and user first; each `AssistantTurn` → one assistant message with `tool_calls` built
  from `ToolCall{call_id, name, raw_arguments}`; each `ToolResult` → tool message; nudge
  messages appear as user messages in order).
- `answer.rs`: write the answer JSON atomically (write `<idx>.json.tmp`, then rename).

**Verify**: `cargo test -p harness-bench trajectory answer prompt` → a scripted two-tool run
produces messages whose roles are `system, user, assistant, tool, tool, assistant`; an
assistant message with null content has no `content` key; tool contents end with `...` and are
at most 503 chars; the written file parses and contains every item-7 key plus `harness_meta`.

### Step 4: MCP execution through 1mcp

`mcp1.rs`: one `McpConnection::connect_http(stage.mcp_url)` per stage; `call(server, tool, args)`
calls `<server>_1mcp_<tool>`; timeout → `Tool call timed out.`; error → `Error: <message>`.
Define a `ToolExecutor` trait so tests and the smoke use `FixtureExecutor` (canned results from
`fixtures/bench-smoke/tool_results.json`, keyed by `<server>_1mcp_<tool>`).

**Verify**: `cargo test -p harness-bench mcp1` → fixture executor returns canned text; a missing
key yields `Error: ...`; timeout path covered with a delay fixture.

### Step 5: Runner, budget, resume, pilot and dry run

`runner.rs`: for each model × stage × condition: build the condition KB once (adaptation calls
billed under `Phase::Adaptation`), then for each task build an `Agent` with the Design settings,
run one turn with the question, write the answer, append events and cost. Skip tasks whose
answer file exists (resume). Ctrl-C finishes the current task's write then exits 4. Implement
`--pilot`, `--dry-run`, `--materialize-full` as specified.

**Verify**: `cargo test -p harness-bench runner` with `MockModelClient` + `FixtureExecutor` +
a tiny KB fixture: answers written for every planned task; rerun writes nothing new; a budget
cap lower than one reservation exits with the budget error before any call; pilot report
projection matches a hand computation in the test; dry run makes zero client calls.

### Step 6: Stage verification command

`harness bench verify-stages --config <file>`: for each stage, connect to its `mcp_url`, call
`tools/list`, convert the tools of the 89 mutated servers to a KB, and diff against
`load_kb(kb_label)` ignoring description-only differences that come from DESC mutations (those
are client-side). Print per stage: servers compared, tools matched, tools only live, tools only
in KB, parameter mismatches; write `stage-verification.json`; set `verified=true` only when
there are no tool-set or parameter mismatches. `harness bench` refuses to run a stage whose
latest verification is missing or failed unless `--allow-unverified-stages` is passed, and
records that flag in the manifest.

**Verify**: unit tests with fixture tool lists: identical → verified; one extra live tool → not
verified with that tool named. (A live check needs 1mcp running; it is a manual step for the
owner, not part of this plan's done criteria.)

### Step 7: CLI wiring and removal of `harness run`

Add to `crates/harness-cli/src/main.rs`:

```
harness bench --config <file> --output <dir> [--run-id <id>] [--models luna,sol,...]
              [--stages early,middle,late] [--conditions ...] [--tasks chosen|0,3,4|sample:N]
              [--effort <level>] [--budget-usd <f64>] [--pilot <N>] [--seed <u64>]
              [--dry-run] [--materialize-full=<bool>] [--allow-unverified-stages]
harness bench verify-stages --config <file> [--output <dir>]
```

Exit codes: 0 ok, 1 setup/config error, 2 usage, 3 some tasks failed, 4 cancelled, 5 budget
exhausted (reuse the constants in commands.rs). Remove the `Run` subcommand, its command
function and `controller.rs` if nothing else uses them; keep `harness validate`. Update
`crates/harness-cli/tests/cli.rs`.

**Verify**: `cargo run -p harness-cli -- bench --help` and `... bench verify-stages --help` →
exit 0; `cargo run -p harness-cli -- run --help` → exit 2 (unknown subcommand);
`cargo test -p harness-cli` → exit 0.

### Step 8: Offline smoke, CI and docs

1. Create `fixtures/bench-smoke/` (a 3-task mini benchmark root with `test_cases.json`,
   `chosen_idx.json`, two depths of `evolution.json`, a `status_dict.json`, an
   `anotation_path/` with two files, mock model scripts, `tool_results.json`) and
   `fixtures/bench-smoke.toml` selecting the mock model and fixture executor. Base its structure
   on `fixtures/mcpevol-mini/` from plan 001. Stages in the smoke config may set
   `allow_unverified = true`.
2. In the CI workflow replace the `harness run` step with
   `cargo run -p harness-cli -- bench --config fixtures/bench-smoke.toml --output target/bench-smoke --budget-usd 0`.
3. In `docs/experiment-protocol.md`, replace the Python runner argv contract with a section
   "Answer-file contract (PROPOSED, pending Person B's agreement)" that states the layout, the
   item-7 fields, `harness_meta`, materialisation, and the two Person B gaps from item 8.
4. Update `README.md` commands (tui, bench, bench verify-stages, validate).

**Verify**: the smoke command → exit 0 and
`find target/bench-smoke -path '*ANSWER/*.json' | wc -l` → the expected count printed by
`--dry-run` on the same config; `python3 -c "import json,sys; [json.load(open(p)) for p in sys.argv[1:]]" $(find target/bench-smoke -path '*ANSWER/*.json')` → exit 0.

### Step 9: Full gates

**Verify**: fmt, clippy and workspace tests exit 0; `git status --short` shows only in-scope
paths; `grep -rn "harness run" README.md docs .github` → no matches.

## Test plan

- `crates/harness-bench/tests/`: `config.rs`, `tasks.rs` (real checkout, skips cleanly when
  absent), `prompt.rs`, `trajectory.rs`, `answer.rs`, `mcp1.rs`, `runner.rs`, `verify.rs`.
- `crates/harness-cli/tests/cli.rs`: bench help, verify-stages help, `run` removed, smoke run
  exit code and file count.
- No test contacts a model provider or a live MCP server.

## Done criteria

- [ ] Workspace fmt, clippy and test gates exit 0
- [ ] Offline smoke exits 0 and writes valid answer JSON files with every item-7 key and `harness_meta`
- [ ] `harness run` no longer exists; CI uses the bench smoke
- [ ] `docs/experiment-protocol.md` contains the PROPOSED answer-file contract and the Person B gaps
- [ ] Involved task counts at depths 2 and 4 are reported
- [ ] No paid call was made (state this in the report; the ledger for every test run shows only mock records)
- [ ] `git status --short` shows changes only under in-scope paths
- [ ] `plans/README.md` row for 004 updated

## STOP conditions

- The benchmark checkout is missing or at a different commit.
- A ported Python function's behaviour is ambiguous (for example `change_test_case_servers`
  on a mutation shape not covered by the code); report the case and the shape.
- `McpEvolAdapter::load_kb` cannot produce a KB for `iter2` or `iter4`, or KB cards cannot be
  matched to task tool names by lineage for more than 5% of chosen tasks; report the counts.
- Any step would need a real API key, a paid call, a download, or an install.
- Removing `harness run` breaks code outside `harness-cli` (report what depends on it).
- The plan 002 agent API cannot express the benchmark stop policy, step cap or tool-result
  truncation.

## Maintenance notes

- The answer-file contract is PROPOSED until Person B agrees; keep it in one module
  (`answer.rs` + `trajectory.rs`) so changes are local.
- Person B must provide `real.json` (or point the judge at `chosen_idx.json`) and run a 1mcp on
  port 3050 for the stage being judged; until then judging fails on their side, not ours.
- Our tool-result string is the joined text content, not Python's `str(list_of_objects)`; this
  is a known, documented difference from the benchmark runner.
- The owner runs, in order: install 1mcp and the bundles (after approval), `verify-stages`,
  `--dry-run`, `--pilot 5` with all models, then decides models, tasks and conditions within the
  budget before any full run.
- Deferred: parallel task execution, container packaging of 1mcp and MySQL, automatic judge
  invocation.
