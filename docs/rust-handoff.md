# Rust and Python handoff

This guide records the Rust/Python exchange and the process adapter now checked into
`benchmark/`. The local contract is implemented. A live pilot still requires matching MCP
servers, a model endpoint, and frozen experiment settings.

## Ownership and shared decisions

| Rust side | Python side | Shared |
|---|---|---|
| TUI, core, CLI, change detection, adaptation strategies, canary and adaptation cost records | Benchmark adapter and runner, completion verifier, judge, scoring, failure analysis and all experiments | Task subset, version mapping, model and judge settings, file formats and final report review |

The Python agent must read the selected Tool KB. Its implementation does not need to run
inside the Rust TUI. The shared runtime formats remain Tool KB JSON and run-event JSONL.
Raw benchmark answer and judge files are supporting evidence, not replacements for these
two contracts. A Rust `bench` command is not required by this ownership split.

## Contract files to share

| File | Purpose |
|---|---|
| `schemas/tool-kb.schema.json` | Exact `tool-kb/1` contract, including required and nullable fields. |
| `schemas/run-event.schema.json` | Exact `run-event/1` contract for each JSONL line. |
| `fixtures/contracts/tool-kb.valid.json` | Small valid KB for adapter development. |
| `fixtures/contracts/events.valid.jsonl` | Valid examples covering every supported event type. |
| `fixtures/contracts/tool-kb.invalid-missing-field.json` | Negative KB example that must be rejected. |
| `fixtures/contracts/events.malformed.jsonl` | Negative log examples that must be rejected. |

Tool cards preserve `lineage_id`, `server_id`, `tool_id`, `name`, `server_version`,
`description`, `input_schema`, `output_schema`, `usage_rules` and `provenance`. The adapter
must use the supplied KB when constructing tools exposed to the model; replacing those
descriptions or schemas with a fresh live listing would change the experimental condition.

Each log line contains the schema version, run ID, sequence, timestamp, source, strategy and
event type, plus the fields of that event type. Fields are flat, unknown keys are rejected,
and nullable required fields must still be present. Blank lines are invalid. Sequence values
must increase within each `(run_id, source)` stream.

The process adapter produces task actions, agent/judge usage, `tf`, `grounding`, and `pe` scores,
and run completion. It does not produce verifier probes/verdicts or failure categories. Keep
`task_id` and `strategy` consistent throughout the exchange. Unknown costs stay `null` with
`cost_status: "unknown"`; they are not zero.

TUI adaptation artifacts live under the session’s `adaptations/drift-.../` directory, shown
in the transcript and TUI. Use `accepted.json` when handing off an explicitly accepted full
session KB. `candidate.json` is only the proposed server subset. Preserve `events.jsonl` and
the local `settings.json`/`state.json` as evidence; only the KB and event log use the shared
contracts. The [README output layout](../README.md#output-layout) describes each artifact.

The supported metric identifiers are `tf`, `grounding`, `pe`, `ecs`, `recovery` and `retention`.
The process adapter emits the first three as task-level judge scores. ECS is calculated from
aligned task-fulfillment events across three version runs, not emitted by one Python stage.
The screenshot's historical scores remain unchanged.

## Local fixture checks

Run from the project root with the pinned Rust toolchain and locked dependencies available:

```sh
cargo test --locked --offline -p harness-core --test contracts
cargo run --locked --offline -q -p harness-core --example check_handoff -- --kb fixtures/contracts/tool-kb.valid.json --events fixtures/contracts/events.valid.jsonl
cargo run --locked --offline -p harness-cli -- validate --config fixtures/smoke.toml
cargo run --locked --offline -p harness-cli -- run --config fixtures/smoke.toml --output target/handoff-smoke
```

The smoke configuration uses mock model responses, a fixture runner and schema-simulated
canary probes. It contacts no provider or external MCP server. It exercises all three
strategies and writes the following layout after a successful run:

```text
target/handoff-smoke/
  manifest.toml
  events.jsonl
  summary.json
  kb/no_update-rep1.json
  kb/full_rewrite-rep1.json
  kb/selective_patch-rep1.json
  python/no_update-rep1.jsonl
  python/full_rewrite-rep1.jsonl
  python/selective_patch-rep1.jsonl
```

The command refuses an existing output run. Use a new output path for another check.
The files under `python/` are prerecorded fixture lines, not fresh Python execution.
The summary includes mock and unknown cost status; fixture values are not experiment results.

These checks establish local fixture behavior. They do not establish that a live server matches
a reconstructed version or that an agent completed a benchmark task. The process-adapter setup
and live-run requirements are in [`benchmark/INTEGRATION.md`](../benchmark/INTEGRATION.md#rust-process-runner).

## Live canary configuration

An MCP server opts into probes through its `mcp.json` entry:

```json
{
  "mcpServers": {
    "disposable": {
      "command": "your-disposable-server",
      "canary": {
        "isolated": true,
        "reset_tool": "reset",
        "reset_arguments": {}
      }
    }
  }
}
```

Use the actual server command and reset tool. `isolated: true` attests that all probe state
is disposable; the reset tool must appear in the current live tool listing. No canary
configuration, false isolation, invalid reset settings or a server mismatch yields an
inconclusive canary without making probe/reset calls. The previous KB remains active.

The TUI executor resets before each probe. Reset failure skips that probe; a tool-declared error
fails the probe. Timeout or transport failure is inconclusive and prevents later calls.
Cancelling stops waiting and skips following probes, but does not undo an operation already
sent to the server. A passing probe establishes accepted execution, not full task completion.

## One shared pilot exchange

1. Agree on one task and exact source/target server versions. Record the task ID, task-file
   hash, server bundle/configuration identity, model, decoding settings, judge and repetition.
2. Rust supplies the selected strategy’s active KB, its hash, the adaptation event log and
   canary outcome. A rejected or blocked candidate must not be presented as promoted.
3. Python loads that KB and records which tools, descriptions and schemas it exposes to the
   model. Confirm the supplied stale KB remains stale for the no-update condition.
4. The process adapter executes the task, then saves the raw answer/trajectory and judge output
   and streams `run-event/1` JSONL. Retain failed or partial runs with their status.
5. Validate the returned JSONL against the schema and review run/task/strategy identity,
   sequence order, completion status and cost fields. Compare actual tool use with the KB.
6. Resolve any mismatch before scaling up. Record the accepted command and files so both
   sides can repeat the same exchange. This is a shared integration check, not the full run.

Run `check_handoff` again with the actual received paths in `--kb` and `--events`. Exit 0
means the artifact checks passed, exit 1 reports an artifact or consistency error, and exit 2
reports invalid arguments. The JSON summary gives card/event counts, run/stream counts,
source event counts and metric event counts. The checker rejects empty logs, unsupported
schemas or metrics, non-increasing sequence values and unresolved canary event references.

The checker does not require a terminal event or verifier verdict. A pass does not prove
run completeness, task success, or that the log was produced using the supplied KB. Those
are the semantic checks in the shared pilot above.

## Information still needed from Python

- Current launch/configuration instructions for the original and evolved MCP servers, and
  confirmation whether they correspond to the Rust adapter's `original/iter1/iter2` labels.
- Model, judge, decoding settings, repetition count, and current rate-card/cost conventions for
  any comparable paid run.
- Status of the separate completion verifier and disposable server reset setup for live canary
  verification.

The local proposed task list differs from the teammate’s screenshot. The new process configs use
the 20 IDs in the imported pilot, while the Rust coverage-based proposal is separate. The
process configs map their version stages to `original/iter1/iter2`; the teammate's historical
results use `original/iter3/iter5`. Matching labels do not establish matching server states.

## Report handoff

Rust supplies the method, TUI and adaptation-cost sections. Python supplies results,
completion verification, failure analysis and discussion. The final report is
[`report/report.pdf`](report/report.pdf).

Fill measured values only from retained run artifacts. The independent local checks and
the shared pilot must be reported separately, with exact commands and their outcomes.
