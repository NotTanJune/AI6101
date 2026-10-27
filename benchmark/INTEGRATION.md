# MCPEvol-Bench integration

The teammate's Python work is integrated into this repository under `benchmark/`.
Source: [Sneha421/MCPEvol-Bench](https://github.com/Sneha421/MCPEvol-Bench/tree/bcc2be50ea1fef8923f0e6b454856eced7dffca6),
commit `bcc2be50ea1fef8923f0e6b454856eced7dffca6`, including both teammate commits after upstream `888c779a0fe7`.
This is a source import into the existing working tree, without a nested Git repository or a Git history merge.

The import contains 719 source files, including all 435 files changed by the teammate, the
20-task pilot, frozen original snapshot, mutation records, trajectories, judgments and patch audits.
`UPSTREAM_IMPORT.json` records every imported file's upstream Git blob and imported SHA-256.
The machine-specific `.idea/workspace.xml` is excluded. Twenty-two credential-shaped samples
in the upstream full task corpus were redacted. The pilot and historical experiment artifacts
were not edited. `LOCAL_CHANGES.json` records subsequent integration edits to imported files.

## Run from the project root

The score audit needs only Python's standard library:

```sh
python3 benchmark/run.py audit
```

The Python runner and converters use a small environment with locked dependencies:

```sh
uv sync --project benchmark --locked
benchmark/.venv/bin/python -m unittest discover -s benchmark/tests -v
benchmark/.venv/bin/python benchmark/run.py --help
```

Use this environment instead of installing the imported `requirements.txt`, which retains
the original machine's large dependency freeze. FastChat source is retained for provenance;
its optional model-serving dependency stack is not installed or tested here. The OpenAI
runner no longer imports FastChat or Torch at startup.

All explicit path arguments to `benchmark/run.py` resolve from the caller's directory.
Legacy default paths still resolve inside `benchmark/src`. Annotation resources resolve to
`benchmark/anotation_path` from any working directory. For live runs, the MCP server must
also be able to access that resource directory.

The original stand-alone scripts can read `OPENAI_API_KEY` and `MODEL_URL` from their
environment or an ignored `.env`. The Rust process adapter receives only the environment
variables named in its config, and sets `PYTHON_DOTENV_DISABLED=1`. No local API credentials
were copied or configured during the import. The later live run is described under
[Live run against the real server versions](#live-run-against-the-real-server-versions).

## Offline patch check

This uses both saved snapshots and disables the model probe. `--dry_run` alone does not
disable adaptation model calls.

```sh
benchmark/.venv/bin/python benchmark/run.py selective \
  --stage early \
  --source_snapshot benchmark/src/custom/data/harness_snapshots/original.json \
  --target_snapshot benchmark/src/custom/data/harness_snapshots/original.json \
  --test_case_file benchmark/src/custom/data/test_cases/pilot_20_clean.json \
  --disable_probe --dry_run \
  --output_path target/benchmark-offline-check
```

The imported `TEAM_HANDOFF.md` records the teammate's original commands and result selection.
For new live experiments, use `benchmark/run.py baseline`, `selective`, or `judge` and new
output directories. Do not overwrite `benchmark_clean_r1` or `benchmark_clean_r2`.
The local judge now uses each saved answer's `involved_tools`, or an explicit `--tool_snapshot`.
It no longer fetches whichever server happens to be running on localhost when imported.
New judgments record model, requested settings, shuffle seed, rubric hash and tool context.
They reject booleans, missing scores and scores outside integer 1 through 10.
These changes affect future evaluations; the historical scores are preserved.

## Tool KB file bridge

Convert the teammate's original snapshot into the shared Rust contract:

```sh
benchmark/.venv/bin/python benchmark/run.py tool-kb from-snapshot \
  --input benchmark/src/custom/data/harness_snapshots/original.json \
  --output target/benchmark-original.tool-kb.json \
  --release-id mcpevol-bcc2be50 --server-version original
```

Convert a Rust KB into the format accepted by Python's `--harness_snapshot` and
`--source_snapshot` options:

```sh
benchmark/.venv/bin/python benchmark/run.py tool-kb to-snapshot \
  --input target/benchmark-original.tool-kb.json \
  --output target/benchmark-original.snapshot.json
```

Conversions validate `schemas/tool-kb.schema.json`, reject duplicate or ambiguous routes,
and preserve descriptions and input schemas. They refuse to overwrite an output file.
The baseline runner preserves frozen schemas, including chart schemas formerly rewritten
by a special case. Input schemas are opaque contract objects: their original dialects are
preserved, not normalized or certified as executable by every validator.

The original snapshot contains 1,384 cards. Metadata absent from legacy snapshots gets explicit
provenance and deterministic route-derived lineage IDs. Those IDs do not establish identity
across renames. Export embeds the complete Tool KB for lossless round trips, but legacy agents
consume only tool names, descriptions and input schemas. Separate output schemas and usage
rules remain metadata; exporting them does not make the legacy agent enforce them.

## Rust process runner

`benchmark/integration/rust_runner.py` is the `ProcessRunner` adapter. It accepts the argv
contract emitted by `crates/harness-core/src/runner.rs`, loads the supplied `tool-kb/1` file,
selects the requested task tools from that KB, runs the imported `ChatModel` against the
configured 1MCP endpoint, and evaluates each saved trajectory with the imported five-assessment
judge. Rust supplies the strategy, task IDs, active server-version label, seed, provider settings,
remaining budget and rate card.

The adapter streams `run-event/1` JSONL records as work completes:

- `model_call` for each agent or judge request, with returned usage and local cost calculation;
- `mcp_action` for each attempted tool call;
- task-level `metric_score` events for `tf`, `grounding` and `pe`;
- `run_complete` for each strategy stage.

The Rust cost ledger ingests the `model_call` events. Paid OpenAI-compatible calls require the
configured rate card and are stopped before dispatch when the child budget cannot reserve the
next request. Unknown usage stops further paid calls. `local` works with an OpenAI-compatible
local endpoint and is recorded as local inference. The imported runner does not support Gemini.
`--mock` fails closed because no deterministic Python agent/tool fixture exists; it does not
produce synthetic scores. The adapter uses the configured model for both agent and judge.

The process adapter connects Rust's strategies and Rust-generated KBs to the imported Python
agent and judge. It does **not** run the teammate's original Python adaptation scripts. A new
combined result therefore evaluates Rust adaptation plus the imported agent/judge, and must be
reported separately from her historical scores. It also does not emit state-based verifier
events. Historical task/version scores are not modified.

### Run the included three-version pilot

The three configs use the same 20 task IDs as the imported pilot and the Rust adapter's version
labels: Early `original`, Middle `iter1`, Late `iter2`. The latter two are cumulative mutation
depths in the supplied snapshot data. They are not verified aliases for the teammate's historical
`iter3` and `iter5` live servers.

Prepare the Python environment and data roots:

```sh
uv sync --project benchmark --locked
export MCP_DATA_DIR="$PWD/benchmark/src/custom/data"
export EVAL_DATA_DIR="$PWD/benchmark/src/custom/data"
```

The configs use `http://127.0.0.1:11434/v1` as a local OpenAI-compatible model endpoint, with
`REPLACE_WITH_LOCAL_MODEL_ID` as a placeholder. Replace that model ID and start the endpoint.
For each command, configure the corresponding live MCP server on
`http://127.0.0.1:3050/mcp` before running:

```sh
cargo run -p harness-cli -- run --config configs/mcpevol-python-early.example.toml --output target/mcpevol-python-early
cargo run -p harness-cli -- run --config configs/mcpevol-python-middle.example.toml --output target/mcpevol-python-middle
cargo run -p harness-cli -- run --config configs/mcpevol-python-late.example.toml --output target/mcpevol-python-late
benchmark/.venv/bin/python benchmark/run.py rust-ecs \
  --early-run target/mcpevol-python-early/events.jsonl \
  --middle-run target/mcpevol-python-middle/events.jsonl \
  --late-run target/mcpevol-python-late/events.jsonl \
  --out target/mcpevol-python-ecs.json
```

Each Rust run evaluates all three strategies and 20 tasks against the target MCP version in
that config. Use separate output directories as shown. The ECS command requires successful
Python stage and Rust run-completion events, and identical task IDs for all strategies across
all three runs. It computes the per-task mean TF minus the population standard deviation across
the three versions, then averages task ECS. It refuses to overwrite the output file.

These configs select a local endpoint so they contain no guessed prices and cannot create a
provider bill. To run with a paid provider, set the real model and API key variable and enter
the current published rates and date in `[budget.rate_card]` first. Do not use placeholder or
zero prices for a paid run. These example configs were not used for the live run, which uses `configs/live/`.

### Live run against the real server versions

This is the configuration used for the live experiment. It replaces the mutation-derived
`original/iter1/iter2` KBs with tool listings captured from the real MCPEvol servers, so each
stage's KB matches the server the agent actually calls: Early is `original`, Middle is `iter3`
and Late is `iter5`. The evolved bundles come from
[anonymous2233/evol-servers](https://huggingface.co/datasets/anonymous2233/evol-servers)
(`iter3_servers.zip`, `iter5_servers.zip`, unpacked under `benchmark/servers/`, git-ignored).

| Step | Command |
|---|---|
| 1MCP configs for the 23 pilot servers | `live_setup.py mcp-config --version <v> [--servers-dir ...] [--node-override mcp-tasks=...]` |
| Capture live tool listings | `benchmark/integration/capture_all.sh original iter3 iter5` |
| Listings to Tool KBs | `benchmark/run.py tool-kb from-snapshot ... --server-version early|middle|late` |
| Rust fixture root | `live_setup.py fixture --early ... --middle ... --late ... --out target/live/fixture` |
| All stages and ECS | `benchmark/integration/run_all_live.sh target/live/runs/full` |
| Report tables | `benchmark/integration/live_report.py --runs target/live/runs/full --out benchmark/reports/live` |

Configs are `configs/live/{early,middle,late}.toml` (provider `openai`, model `gpt-6-luna`,
key variable `OPENAI_ETHICS_KEY`, dated rate card). The table above lists the commands.
Operational constraints found while connecting the live path:

- 1MCP is pinned to 0.38.3. Versions 0.39.x rewrite public tool names (for example
  `@umengfe/mcp-server-chart` becomes `_umengfe/mcp-serve`), which breaks `<server>_1mcp_<tool>`
  routing. With 0.38.3 the live `original` listing is byte-identical to the frozen snapshot.
- `mcp-tasks` fails under an unpinned `npx` install (a transitive module is missing); the
  original and iter3 configs run the bundled copy instead.
- Rust places its configured fixed arguments before `run`; the runner accepts that order and
  exits 64, not 2, on usage errors, because exit 2 means budget exhaustion in the Rust contract.
- The Early stage sets `versions.baseline = true` because source and target are the same version.
- Rust's selective patch keeps unaffected cards byte-identical to the source, so their
  `server_version` stays at the source label. The runner validates KBs with
  `uniform_versions=False`; the snapshot converters keep the strict check.

## Integration boundaries

| Area | Current state |
|---|---|
| Source, datasets, historical results | Present in this repository and audited offline |
| Tool KB interchange | Snapshot conversion validated against the shared contract and Rust checker |
| Rust process runner | Implemented by `integration/rust_runner.py`; it consumes Rust's active Tool KB and emits Python-source `run-event/1` JSONL |
| Live server execution | Run end to end on 2026-10-26/27 against real original, iter3 and iter5 servers through 1MCP 0.38.3 (180 task runs, all stages status ok). Imported `servers/1mcp_config/*/mcp.json` still needs machine paths, which `live_setup.py mcp-config` fills |
| Budget and events | Agent/judge usage and tool actions flow to Rust; paid dispatch is rate-card metered and capped. Gemini is unsupported by the imported OpenAI client |
| Completion verifier | No separate state-based task completion verifier is supplied by these experiment artifacts |

The adapter and contract are implemented and were exercised end to end in the live run
(results in `benchmark/reports/live/`). The adapter validates Python events against the shared schema. `grounding` is now a supported
`run-event/1` metric and is emitted with the other per-task judge scores.

The existing Rust example mapping remains `original/iter2/iter4`. The new three-run configs use
`original/iter1/iter2` so their task-level logs align with Early/Middle/Late in this process
example. The teammate's historical scores use `original/iter3/iter5`, as recorded in
`TEAM_HANDOFF.md`; the state equivalence is not established. The combined results must not be
merged with the historical score table as if they were the same experiment.

Python's selective probe generates arguments and validates their schema without executing a
tool. Rust's live canary performs MCP calls. Python's full rewrite takes current server tool
definitions; Rust's full rewrite regenerates KB content through an adaptation model.
The imported scores therefore evaluate the Python implementation. They do not validate the
Rust canary, verifier, budget enforcement or strategy implementation.

## Results and integrity

- [Verified scores and interpretation](../docs/benchmark-results.md)
- [Machine-readable audit](reports/verified-results.json) and [score table](reports/verified-results.csv)
- `python3 benchmark/integration/verify_import.py` checks every imported file against its
  recorded import hash or a documented integration edit.
- `python3 benchmark/run.py audit --out benchmark/reports/verified-results.json` regenerates
  the JSON/CSV from the selected historical artifacts, including source hashes.

The ECS script's missing-file metadata had a set-operator precedence error. It is corrected
locally. The ECS calculation is unchanged; all three committed ECS values reproduce.
