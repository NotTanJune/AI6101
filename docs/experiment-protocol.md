# Experiment protocol

Original status date: 2026-10-03. Integration update: 2026-10-26. Scope: plan
`plans/001-mcp-harness-tui.md`, Steps 1 and 7.

This document records what the supplied benchmark data gave us, what the adapter does with it,
and what is still open. Every number in sections 1 and 2 was produced by the commands in
section 13 against the data described in section 1. Nothing is invented. Where a value is a
decision, the cell says so explicitly.

## 0. Read this first: what is and is not known

Known now (from the supplied MCPEvol-Bench checkout, section 1):

- The input files, their SHA-256 values, and their counts.
- The version labels and the stage mapping. The mapping is a project decision (section 1.3).
- The stage by change-class coverage of the 201 evaluation tasks (section 2.1).
- A PROPOSED pilot subset of 20 tasks (section 2.2). It is not frozen.

Still open:

- The pilot is a proposal. The plan requires the project owner to review pilot cost before the
  task set and settings are frozen. No combined live pilot has been run.
- Model, decoding settings, judge version, repetition count, and rate card are project-owner
  decisions. They are shown as `TO BE DECIDED BY PROJECT OWNER`.
- The Rust process contract and `benchmark/integration/rust_runner.py` adapter are implemented
  and documented in section 6. Live evaluation still needs configured MCP server versions and
  frozen provider settings. No end-to-end live run has been verified.
- The adapter reconstructs server versions from a snapshot. It does not have the real evolved
  server definitions (section 1.5). Results on `original`, `iter1`, and `iter2` are
  reconstructions, not live server definitions.

Legend used in this document:

| Marker | Meaning |
|--------|---------|
| `BLOCKED` | Cannot be done with what this repository has. The cell names the missing input. |
| `TO BE DECIDED BY PROJECT OWNER` | A decision, not a measurement. Nobody else may fill it in. |
| `PROPOSED, NOT FROZEN` | A concrete proposal from the data. The owner reviews it before it becomes the protocol. |
| `IMPLEMENTED` | Exists in the repository and is covered by the automated checks named next to it. |

## 1. Supplied inputs

The supplied inputs are immutable and stay outside Git. Roots are read from environment
variables; absolute paths are never written into committed files. The benchmark is used as it
is: the TA asked for the whole MCPEvol-Bench repository, and its own counts are authoritative.

| Item | Value |
|------|-------|
| Catalog root | `$MCP_DATA_DIR`, set to the benchmark clone's `src/custom/data` directory |
| Evaluation root | `$EVAL_DATA_DIR`, set to the same `src/custom/data` directory |
| Benchmark | MCPEvol-Bench, <https://github.com/Octobrist/MCPEvol-Bench> |
| Benchmark release ID | `mcpevol-888c779a0fe735f612ca62bdd1fdd7c9e6bf8701` |
| Release version or date | Only the commit id `888c779a0fe735f612ca62bdd1fdd7c9e6bf8701` was recorded. The adapter derives the release ID from the checkout's git HEAD when the catalog directory is `<clone>/src/custom/data`; for any other layout it uses a hash of the input file hashes. |
| Redistribution approved by input owner | Not recorded. Until approved, the inputs stay out of Git and no benchmark content is copied into this repository. |

### 1.1 Input files and checksums

Seven files are read. The catalog root and the evaluation root are the same directory in the
verified setup. Names are relative to that directory.

| Root | Source-relative file name | SHA-256 | Bytes |
|------|---------------------------|---------|-------|
| `MCP_DATA_DIR` | `test_cases/status_dict.json` | `77909019530c11a0da21d0ffd977fd31ee23db5bf8c4f61b4f254d2857b9a7e9` | 2,635,683 |
| `MCP_DATA_DIR` | `mutation/HYBRID/evolution.json` (depth 1) | `26d3e98c1464ab8b784bc37c3e7dc2c223038d1d3f06931cee56b76fb01acd46` | 472,773 |
| `MCP_DATA_DIR` | `mutation/HYBRID/HYBRID/evolution.json` (depth 2) | `9186e5959df32b933bf88689d71454e98793bfcaf4c8b65478cc24ba2abf5a08` | 456,127 |
| `MCP_DATA_DIR` | `mutation/HYBRID/HYBRID/HYBRID/evolution.json` (depth 3) | `d78c551f0136a7b0ad17fcb519e27aeb14b0b885fcb0a6309e98b2526bdc9431` | 651,169 |
| `MCP_DATA_DIR` | `mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json` (depth 4) | `43d7deba235cb60190ae8db805235551cde015f71f81829d374ec49b14418d2b` | 544,246 |
| `EVAL_DATA_DIR` | `test_cases/test_cases.json` | `b51d09d93afa94b08a67281a95ee8b4c7d69953816adf6769a996e26274bb023` | 15,847,946 |
| `EVAL_DATA_DIR` | `test_cases/chosen_idx.json` | `55df803b7cf40fe1a40d27ff6b2375d87b62e308fbdbc6da3e27e6adbcb63d6c` | 8,802 |

`harness validate` prints the same seven hashes, and every `harness run` records them in
`manifest.toml`.

### 1.2 Counts

The project brief described a release with 123 servers, 1,272 tools, and 201 tasks. Those
numbers are informational. The repository's own counts are authoritative, and the difference
is not an error.

| Quantity | Expected per project brief (informational) | Observed in the repository | Counting rule |
|----------|--------------------------------------------|----------------------------|---------------|
| MCP servers | 123 | 163 | Top-level keys of `status_dict.json` |
| Tools | 1,272 | 1,685 | Entries of every server's `tools` array in `status_dict.json` |
| Test pool | not stated | 393 | Entries of `test_cases.json` |
| Evaluation tasks | 201 | 201 | Entries of `chosen_idx.json` (`chosen_idx` selects the 201 tasks) |

The 201 tasks name 128 distinct server names:

- 124 names resolve to `status_dict` keys, 4 of them by the suffix rule in section 1.5. These
  124 names map to 120 distinct keys (several names point at the same key).
- 4 names do not resolve: `fixparser-plugin-mcp`, `google-cloud/storage-mcp`,
  `mcp-sequential-thinking`, `nfty-mcp-server` (one task each). Their servers are excluded
  from the Tool KB. The tasks stay in the inventory.
- 3 tools named by tasks exist in no reconstructed version:
  `@jinzcdev/leetcode-mcp-server/get_daily_challenge` (1 task),
  `mcp-knowledge-graph/create_entities` (2 tasks), `mcp-knowledge-graph/create_relations`
  (1 task).

Evolution entries per cumulative depth 1 to 4: 89, 86, 85, 87 (347 in total).

### 1.3 Version labels and stage mapping

The adapter exposes five labels: `original`, `iter1`, `iter2`, `iter3`, `iter4`. `iterN` means
evolution files 1 to N applied cumulatively, oldest first. The number N is the cumulative depth.

The stage mapping is a PROJECT DECISION, configurable under `[versions.stages]`:

| Stage | Label | Cumulative depth | What the label means |
|-------|-------|------------------|----------------------|
| Early | `original` | 0 | The reconstructed original: the snapshot with evidenced changes rewound (section 1.5). No evolution applied. |
| Middle | `iter2` | 2 | The reconstructed original plus evolution files 1 and 2. |
| Late | `iter4` | 4 | The reconstructed original plus evolution files 1 to 4. |

`iter1` and `iter3` are available as labels but are not stages in the default mapping. The
adapter rejects a mapping that goes backwards (early after middle after late) or names a
label that does not exist.

Tool counts in the scoped Tool KB for all 201 tasks (cards for the tools the tasks name, plus
tools that evolutions add to those servers): `original` 310, `iter1` 311, `iter2` 339,
`iter3` 352, `iter4` 352. These are not whole-catalog counts.

Other items of the earlier checklist:

| Item | Value |
|------|-------|
| Stable lineage ID | Not in the release. The adapter builds `lineage_id = <server>::<original tool name>` (section 1.5). |
| Output-schema changes | Not modeled. The evolution files have no structured output-schema field (the word appears only inside embedded source text). Output schemas of existing tools are copied from the snapshot. |
| Tasks per stage (Early / Middle / Late), all 201 tasks | 15 / 166 / 20 |
| Tasks per change class (Description / Parameter / Tool), all 201 tasks | 140 / 122 / 109 (a task can have several classes, so the three numbers sum to more than 201) |

### 1.4 What `status_dict.json` is

`status_dict.json` is not the original tool catalog. It is a snapshot of live servers that
already contains most structural evolutions of depths 1 to 3. For parameter additions, the
number present in the snapshot out of the number in the evolution file is:

| Depth | Added parameters present in the snapshot |
|-------|------------------------------------------|
| 1 | 72 of 84 |
| 2 | 35 of 50 |
| 3 | 96 of 114 |
| 4 | 3 of 61 |

Description changes are not baked in. The adapter therefore cannot treat the snapshot as
`original`. It rewinds evidenced structural changes first (section 1.5), so `original`,
`iter1`, and `iter2` are RECONSTRUCTIONS and not live server definitions. The default is
`rewind_baseline = true`.

`task_idx` inside an evolution entry does not identify our tasks: only 11 of 347 entries
point at a task that lists the mutated server. The adapter never uses it (section 1.5).

### 1.5 Adapter design (`mcpevol`, `crates/harness-core/src/mcpevol.rs`)

The adapter is read-only. It never writes into either data directory.

**Rewind baseline.** Walking depth 4 down to 1, every mutation whose result is evidenced in
the snapshot is undone: an added parameter that is present is removed, an added tool that is
present is removed, a description equal to the modified text is restored to the original text,
and a required flag equal to the new state is restored. The removed definitions are remembered
so that the forward replay reinserts the snapshot's exact definition. Mutations whose result is
not evidenced are left alone. Removed parameters that are absent from the snapshot are restored
only from what the mutation itself records.

**Forward replay.** `original` is the rewound baseline. `iterN` applies files 1 to N in order,
cumulatively. Every card carries `provenance.source_pointer`, listing the snapshot location
and every replayed mutation.

**Involvement rule.** Tasks are linked to mutations by the benchmark runner's own rule, not by
`task_idx`. A mutation involves a task when the mutated server is in `task.servers` and the
mutated tool is in the task's tool list for that server. A TOOL add always involves the
server's tasks. A TOOL delete involves a task when the deleted tool is in the task's list.

**Task stage and change classes.** A task's change classes are the classes of the mutations
that involve it, up to the Late depth. Its stage is the earliest stage whose cumulative depth is
at least the mutation depth, taking the minimum over all involving mutations. A task is Early
only when no mutation involves it. Consequence: Early (`original`, depth 0) contains no changes
by construction, so the Early row of the coverage matrix can only hold unchanged control tasks.
That is a property of the stage mapping and of how mutations are dated, not a data gap.

**Name resolution rule.** A task or evolution server name resolves to a `status_dict` key when
it equals the key. Otherwise it resolves when exactly one key equals `@<name>` or ends with
`/<name>`. No match, or several matches, leaves the name unresolved. Unresolved names are
reported, and their servers are excluded from the KB. The four suffix resolutions are:

| Name in the data | Resolved key |
|------------------|--------------|
| `mcp-server-time` | `@guanxiong/mcp-server-time` |
| `pollinations/model-context-protocol` | `@pollinations/model-context-protocol` |
| `server-memory` | `@modelcontextprotocol/server-memory` |
| `server-sequential-thinking` | `@modelcontextprotocol/server-sequential-thinking` |

**Lineage rule.** `lineage_id = <server>::<original tool name>`. A tool introduced by a TOOL
mutation gets its own lineage. A deleted tool is a removal. `migration_target_tool` is
documentation of where behavior moved to an existing tool and does not create lineage. Lineage
survives a rename only for the explicit `old_tool_name` plus `new_tool_config` shape; the
supplied data has no instance.

**Unmapped and unresolved reporting.** Whatever the adapter cannot map faithfully is not
applied and not guessed. It is listed in the report and summarized by `harness validate`. The
108 unmapped items by reason:

| Reason | Items |
|--------|-------|
| `optimized_description_text_unavailable` | 87 |
| `free_text_change` | 10 |
| `tool_definition_unavailable` | 8 |
| `required_text_not_mapped` | 1 |
| `server_not_in_status_dict` | 1 |
| `tool_not_present` | 1 |

`validate` also prints 371 replay notes by kind: `from_snapshot` 266, `base_mismatch` 79,
`reconstructed` 15, `already_applied` 5, `loose_type` 3, `not_reconstructable` 2,
`loose_schema` 1.

**Fidelity limits.**

- Free-text constraint and type changes, and optimized description text, are not reproduced.
- Parameters removed by a mutation and restored from the mutation record have only the declared
  type, not the full original schema.
- Tools deleted before the snapshot cannot be restored.
- A fidelity probe against the snapshot (`crates/harness-core/tests/mcpevol_real.rs`) found 299
  of 352 `iter3` input schemas equal to the snapshot and 53 different. `iter3` is the version
  structurally closest to the snapshot, so these 53 mark where the reconstruction and the
  snapshot disagree.
- True evolved definitions need the server bundles that the benchmark publishes separately
  (Hugging Face dataset `anonymous2233/evol-servers`). This project does not use them.

## 2. Pilot design

Rules (fixed by the plan, independent of the data):

- The pilot is a subset of 20 to 30 evaluation tasks.
- It should cover Tool, Parameter, and Description changes across Early, Middle, and Late. A
  cell with no available task is listed as uncovered with a reason, not silently dropped.
- It is used first as the no-update baseline to estimate cost.
- The implementation executor did NOT run a real-provider pilot. The project owner runs it.
- After the owner reviews the pilot cost, the task set and all settings are frozen (section 3)
  and used for every strategy.
- Excluded tasks are listed with a reason.

### 2.1 Coverage matrix over the 201 evaluation tasks

Produced by `cargo run -q -p harness-core --example pilot_candidates` (section 13). A task
counts once in every class it has, so a row can sum to more than its task count. The last
column counts tasks with no class.

| Stage | Tasks | Tool | Parameter | Description | No change |
|-------|-------|------|-----------|-------------|-----------|
| Early (`original`) | 15 | 0 | 0 | 0 | 15 |
| Middle (`iter2`) | 166 | 105 | 121 | 121 | 0 |
| Late (`iter4`) | 20 | 4 | 1 | 19 | 0 |

How to read it:

- The Early row is controls only (section 1.5). The three Early change cells are empty by
  construction. They are not an inventory gap, and they cannot be filled under this mapping.
- A task's stage is its earliest involving mutation. A Middle task can therefore also carry a
  class that comes from a depth 3 or 4 mutation. A Late task has only mutations at depth 3 or 4.
- Late x Parameter has exactly one task (id 127). No selection can reach 3 there.

### 2.2 Proposed pilot task IDs (PROPOSED, NOT FROZEN)

The selection is deterministic and lives in `crates/harness-core/src/pilot.rs` (`select_pilot`).
The rule:

0. Leave out tasks that name an unresolved server or tool (section 1.2). They are not eligible.
   Leave out tasks 299 and 401 as well: their external servers crash on a pilot run (section 2.3).
   The rest of the rule uses eligible tasks only.
1. Take every non-empty (stage, class) cell. Process them scarcest first (fewest member
   tasks), ties by stage then class order.
2. Give each cell at least 3 tasks, counting tasks already chosen for earlier cells. Within a
   cell take the cheapest task first. Cheapest means fewer servers listed by the task, then
   fewer tools listed by the task, then the lower numeric task id.
3. Add 5 Early control tasks (stage Early, no change class), cheapest first.
4. While the total is below 20, give the cell with the fewest chosen tasks its cheapest
   unchosen task (ties: the scarcer cell). Stop at 20. The hard maximum is 30.
5. Report every cell that stayed below 3, with the number of eligible tasks it has.

The cost proxy is the count of servers and tools the task itself lists. It is not a measured
cost; it does not include the task's model calls or the length of its trajectory.

```toml
[tasks]
ids = ["5", "6", "12", "26", "29", "30", "64", "78", "92", "127", "225", "229", "235", "249", "273", "281", "291", "385", "440", "448"]
```

Coverage matrix of these 20 tasks:

| Stage | Tasks | Tool | Parameter | Description | No change |
|-------|-------|------|-----------|-------------|-----------|
| Early | 5 | 0 | 0 | 0 | 5 |
| Middle | 8 | 5 | 5 | 5 | 0 |
| Late | 7 | 3 | 1 | 6 | 0 |

Cells below 3: Late x Parameter has 1 task chosen and 1 eligible task in the whole inventory.
The three Early change cells are empty in the inventory.

Per-task detail. "Servers" and "Tools" are the counts the task lists.

| Id | Stage | Servers | Tools | Classes | Chosen for |
|----|-------|---------|-------|---------|------------|
| 5 | Middle | 2 | 2 | Description | Middle x Description |
| 6 | Late | 2 | 3 | Description | Late x Description |
| 12 | Late | 2 | 3 | Description | balance, Late x Description |
| 26 | Middle | 2 | 3 | Tool, Parameter | balance, Middle x Tool |
| 29 | Middle | 2 | 2 | Tool, Parameter, Description | Middle x Tool |
| 30 | Middle | 2 | 2 | Tool, Parameter, Description | Middle x Tool |
| 64 | Middle | 2 | 2 | Description | balance, Middle x Description |
| 78 | Middle | 2 | 2 | Description | balance, Middle x Description |
| 92 | Early | 2 | 3 | none | Early control |
| 127 | Late | 2 | 3 | Tool, Parameter | Late x Parameter |
| 225 | Middle | 2 | 2 | Tool, Parameter | Middle x Tool |
| 229 | Middle | 2 | 2 | Tool, Parameter | balance, Middle x Tool |
| 235 | Early | 1 | 3 | none | Early control |
| 249 | Early | 1 | 2 | none | Early control |
| 273 | Late | 2 | 4 | Description | balance, Late x Description |
| 281 | Late | 3 | 5 | Tool, Description | Late x Tool |
| 291 | Early | 1 | 2 | none | Early control |
| 385 | Late | 3 | 4 | Tool, Description | Late x Tool |
| 440 | Late | 2 | 3 | Description | balance, Late x Description |
| 448 | Early | 1 | 2 | none | Early control |

Sum over the pilot: 38 server slots and 54 tool slots.

This is a proposal for the project owner to review. Changing the rule's parameters (cell
target, controls, total) or `EXCLUDE_FOR_CAUSE` in
`crates/harness-core/examples/pilot_candidates.rs` changes the ids; rerun the example and
replace this section. The pilot is not frozen until the owner has reviewed the cost of the
no-update baseline run.

### 2.3 Excluded tasks

Excluded because they name an unresolved server or tool (section 1.2): 7 tasks, ids 8, 74, 85,
89, 90, 112, 289. The Tool KB lacks those servers or tools, so it cannot represent these tasks
fully. They stay in the 201-task inventory.

Excluded because their external servers crash on a pilot run: ids 299 and 401. They stay in the
201-task inventory. The selector skips them and fills the 20 from the remaining eligible tasks.
Those ids are `EXCLUDE_FOR_CAUSE` in `crates/harness-core/examples/pilot_candidates.rs`.

The other 172 tasks outside the 20 proposed ids are not in the pilot, and they are not excluded
for a defect. The owner may also decide to keep the 7 unresolved tasks above.

## 3. Frozen settings (identical for all strategies)

The comparison is only valid if the same base model, decoding settings, task set, and judge
are used for no update, full rewrite, and selective patch. Fine-tuning any model, changing
benchmark labels, or changing these settings between strategies is out of scope.

| Setting | Value |
|---------|-------|
| Base model and provider | `TO BE DECIDED BY PROJECT OWNER` |
| Temperature | `TO BE DECIDED BY PROJECT OWNER` |
| Top-p | `TO BE DECIDED BY PROJECT OWNER` |
| Max output tokens | `TO BE DECIDED BY PROJECT OWNER` (required for paid providers so calls can be reserved) |
| Seed | `TO BE DECIDED BY PROJECT OWNER` |
| Judge version | `TO BE DECIDED BY PROJECT OWNER` (the supplied benchmark's judge; the version has not been recorded) |
| Task set | Section 2.2 is PROPOSED, NOT FROZEN |
| Server version stages | Early = `original`, Middle = `iter2`, Late = `iter4` (project decision, section 1.3) |
| Repetitions | `TO BE DECIDED BY PROJECT OWNER`: 1 or 3, chosen after the pilot cost review. Record the reason (budget). |
| Rate card ID | `TO BE DECIDED BY PROJECT OWNER` |
| Rate card date | `TO BE DECIDED BY PROJECT OWNER` (the date the prices were taken) |
| Rate card entries | `TO BE DECIDED BY PROJECT OWNER`. Paid calls are blocked while no rate exists for the provider and model. |
| Local dispatch cap | Configured in `[budget].cap_usd`. The project plan targets approximately USD 100; the frozen value is `TO BE DECIDED BY PROJECT OWNER`. |

Rule: results from runs whose settings differ must be labeled as different experiments. Never
compare them as if they were one comparison.

## 4. Shared interchange contracts

Only two files cross the Rust/Python boundary. There is no third shared format. Both are
versioned JSON Schema (draft 2020-12) documents checked into `schemas/`, embedded into the Rust
crate, and exercised by the fixtures in `fixtures/contracts/`. Unsupported versions fail closed.

### 4.1 Tool KB, `tool-kb/1` (`schemas/tool-kb.schema.json`)

A JSON document describing tool cards for one server version.

Top level (all required, no extra keys): `schema_version` (`"tool-kb/1"`), `kb_id`,
`release_id`, `server_version`, `cards`.

Each card (all required, no extra keys):

| Field | Type | Notes |
|-------|------|-------|
| `lineage_id` | string | Stable id for a tool across versions; renames keep it. |
| `server_id` | string | |
| `tool_id` | string | Source-native tool id. |
| `name` | string | |
| `server_version` | string | Version this card describes. |
| `description` | string | |
| `input_schema` | object | JSON Schema of the input. |
| `output_schema` | object or null | Null when the source exposes none. |
| `usage_rules` | array of string | |
| `provenance` | object | `release_id`, `source_file` (relative to the catalog root), `source_pointer` (string or null). |

Rust serializes deterministically (object keys sorted, compact); a card hash is the SHA-256 of
its canonical JSON. The selective-patch boundary check relies on that.

### 4.2 Run event JSONL, `run-event/1` (`schemas/run-event.schema.json`)

One JSON object per line. Every line is independently valid. Blank lines are rejected. An error
names the 1-based line number.

Envelope on every event (all required): `schema_version` (`"run-event/1"`), `run_id`, `seq`
(monotonic per `run_id` and `source`), `ts` (RFC 3339), `source` (`rust` or `python`),
`strategy` (`no_update`, `full_rewrite`, `selective_patch`, or null), `event_type`. Body fields
are inline (flat) next to the envelope. Each variant forbids unknown keys.

| `event_type` | Body fields | Typical producer |
|--------------|-------------|------------------|
| `config` | `provider`, `model`, `judge_version`, `seed`, `repetition`, `budget_cap_usd` (or null), `task_ids`, `source_version`, `target_version`, `temperature`, `top_p`, `max_output_tokens` (each nullable where marked) | Rust |
| `adaptation_diff` | `source_version`, `target_version`, `changes` (list of change records), `candidate_hash` (or null) | Rust |
| `adaptation_outcome` | `status` (`promoted`, `blocked`, `unchanged`, `rejected`), `reason`, `candidate_hash`, `prior_hash`, `changed_lineage_ids` | Rust |
| `model_call` | `provider`, `model`, `phase` (`adaptation`, `canary`, `verifier`, `probe_generation`, `agent`, `other`), `request_id`, `input_tokens`, `output_tokens` (nullable), `latency_ms`, `rate_card_id`, `rate_card_date`, `estimated_usd` (nullable), `cost_status` (`priced`, `unknown`, `mock`, `local`) | Rust and Python |
| `mcp_action` | `task_id`, `step`, `tool_id`, `arguments`, `result_summary`, `is_error` | Python, and Rust for canary probes |
| `canary_result` | `status` (`pass`, `fail`, `inconclusive`, `not_applicable`), `tool_id`, `probe_id`, `is_control`, `expected`, `actual`, `event_refs` (seq numbers of the related `mcp_action` events) | Rust |
| `verifier_probe` | `task_id`, `probe_id`, `description`, `observation` | Python |
| `verifier_verdict` | `task_id`, `outcome` (`confirmed_success`, `confirmed_failure`, `inconclusive`), `claimed_complete`, `rationale`, `probe_ids` | Python |
| `metric_score` | `metric` (`tf`, `pe`, `ecs`, `recovery`, `retention`), `task_id` (or null), `value` | Python |
| `failure_category` | `task_id`, `category` (`under_update`, `over_update`), `detail` | Python |
| `error` | `message`, `fatal` | Rust and Python |
| `run_complete` | `status` (`ok`, `failed`, `cancelled`, `budget_exhausted`), `detail` (or null) | Rust and Python |

A change record has `lineage_id`, `tool_id`, `class` (`tool`, `parameter`, `description`),
`kind` (`added`, `removed`, `renamed`, `modified`), `field_path`, `old_hash`, `new_hash`.

The metric ids `tf`, `pe`, `ecs` are opaque identifiers owned by the Python workstream. Rust
carries and displays them and never computes them.

Schema field changes require a new schema version (see `CONTRIBUTING.md`).

## 5. Ownership boundary

| Rust workstream (this repository) | Python workstream (Person B, not in this repository) |
|-----------------------------------|------------------------------------------------------|
| Input validation and normalization to Tool KB cards (the `mcpevol` adapter reads the supplied release) | Task execution against MCP servers |
| Version diffing and change classification | Completion verifier and false-completion detector |
| The three adaptation strategies and patch validation | Verifier and probe-generation model calls (must emit `model_call` events) |
| Provider interface (OpenAI, Gemini, local OpenAI-compatible, mock) | Score computation (`tf`, `pe`, `ecs`, recovery, retention) |
| Canary gate and promotion or rollback | Failure analysis and categorization events |
| Cost ledger, reservations, the local dispatch cap | Per-call cost events with the same fields |
| Event emission, redaction, schema validation, run output layout | Analysis scripts that recompute from the raw JSONL |
| TUI and headless CLI over one pipeline | The runner executable itself |
| Process launcher and JSONL parser for the runner | |

Rust must not reimplement benchmark task execution, verification, judging, scoring, or failure
analysis. No Python files live in this repository.

## 6. Rust-to-Python process runner (IMPLEMENTED, live run pending)

Rust `ProcessRunner` launches `benchmark/integration/rust_runner.py`. The adapter consumes the
active Rust Tool KB and uses the imported Python agent and judge. Rust owns KB adaptation and
canary execution; this adapter does not run the teammate's original Python adaptation scripts.
The new combined results must remain separate from the teammate's historical Python scores.
No end-to-end live run has been performed. Live evaluation still requires a matching MCP server
and frozen provider settings. See `benchmark/INTEGRATION.md` for the run procedure.

### 6.1 Invocation

The runner is spawned with an executable and an argument array, never through a shell. The
configured `fixed_args` come first, then the following arguments.

```text
<executable> [fixed_args...] run
  --run-id <id>
  --strategy <strategy>
  --tool-kb <path>
  --server-version <active-kb-label>
  --source-version <label>
  --eval-data-env <env-name>
  [--api-key-env <env-name>]
  --task-id <id>            (repeated once per task)
  --provider <name>
  --model <id>
  [--temperature <f>] [--top-p <f>] [--max-output-tokens <n>]     (omitted when unset)
  --seed <n>
  --judge-version <v>
  --output-jsonl <path>
  --remaining-budget-usd <f>
  [--rate-card-json <json>]
  [--mock]                                                         (adapter fails closed)
```

`--tool-kb` is the `tool-kb/1` file for the strategy being evaluated. `--output-jsonl` is the
path for `run-event/1` lines, tailed while the child runs. `--remaining-budget-usd` is the
remaining run cap after Rust adaptation calls. The adapter emits `model_call`, `mcp_action`,
task-level `metric_score` events for `tf`, `grounding`, and `pe`, then `run_complete`.
`--server-version` identifies the active KB file, which may remain at the source version if an
adaptation is blocked. `--source-version` and `--eval-data-env` preserve run context. The
adapter supports `openai` with a matching rate-card entry and OpenAI-compatible `local`; it
rejects `gemini` because the imported agent uses the OpenAI chat-completions client.

### 6.2 Environment

| Variable | Purpose |
|----------|---------|
| `MCP_DATA_DIR` | Catalog root (name configurable via `[inputs].catalog_env`) |
| `EVAL_DATA_DIR` | Evaluation root (name configurable via `[inputs].eval_env`) |
| Provider key variable, for example the name in `[provider].api_key_env` | Credential; value never appears in argv, config, logs, or events |
| `MODEL_URL` | Provider base URL, passed when `[provider].base_url` is configured |
| `PYTHON_DOTENV_DISABLED` | Set to `1` in the child so imported dotenv helpers cannot load a local credential file |

The child receives a cleared environment plus a small base (`PATH`, `HOME`, `USER`, locale
variables, temp directory variables, `SYSTEMROOT`, `VIRTUAL_ENV`, `PYTHONHOME`, `PYTHONPATH`) plus
only the variables explicitly named for passthrough.

### 6.3 Exit codes

| Code | Meaning |
|------|---------|
| 0 | Stage completed |
| 2 | Child budget exhausted; partial JSONL is retained and Rust stops the matrix |
| 1 | Adapter error; partial JSONL is retained and the Rust stage fails |

### 6.4 Parsing rules

Rust validates every line against `run-event/1` while the child runs. A malformed or
unsupported line fails closed with an error naming the line. Human-readable console output of
the child is never parsed. Raw lines are preserved verbatim in the run output directory.

### 6.5 Remaining live-run requirements

- Install and configure each target MCP server on the endpoint used by the runner. The imported
  MCP config files contain machine-specific paths and do not provide portable server bundles.
- Freeze a provider/model, decoding settings, rate-card values for paid calls, judge label,
  repetition count, and task set. The checked-in process examples use a placeholder local model.
- Verify one live task and retain its Tool KB, answer, judge artifact, raw JSONL, and Rust events.
- A separate state-based completion verifier remains unavailable. The adapter does not emit
  verifier events or treat a normal agent stop as proof of task completion.

## 7. Canary versus completion verifier

These are different mechanisms with different owners and must not be merged.

| | Canary | Completion verifier |
|-|--------|---------------------|
| Question answered | Did the recent adaptation break or fail to fix the changed tool? | Did the agent actually complete the user's original request, and was its completion claim true? |
| Owner | Rust | Python (Person B) |
| When | Once per candidate patch, before promotion | After each task attempt |
| Inputs | The changed tool(s) in the target server version | The original request and the benchmark state |
| Size | 1 to 3 small targeted probes, plus at most one unchanged control probe when available | Request-grounded probes chosen by the verifier |
| Result events | `canary_result`: `pass`, `fail`, `inconclusive`, `not_applicable` | `verifier_probe` and `verifier_verdict`: `confirmed_success`, `confirmed_failure`, `inconclusive` |
| Effect | A candidate is promoted only after a pass. `fail` or `inconclusive` keeps the prior KB and marks the candidate blocked. No-update runs record `not_applicable`. | Feeds scoring and failure analysis; does not gate promotion |

Safety rules for canary probes: run them only against isolated, resettable benchmark state, and
never issue a mutating probe against non-disposable state. If isolation cannot be guaranteed,
the probe is not executed and the result is `inconclusive`. If the real benchmark state cannot
be isolated at all, stop and report (plan STOP condition).

## 8. Cost accounting rules

- Every paid or local model request made by adaptation and canary produces a `model_call`
  event. The Python runner must emit the same fields for its own model requests.
- Unknown usage or unknown price is `unknown`, never zero.
- Mock usage is labeled `mock` and never counted as USD. Local model usage is labeled `local`,
  its latency is recorded, and it is tracked separately from paid API cost.
- Paid calls are blocked when no rate is configured for the provider and model. A max output
  token limit is required for paid providers.
- Before each paid call the ledger reserves a conservative cost from the rate card and the
  request size, and refuses dispatch if spent plus unreconciled plus outstanding reservations
  plus this reservation would exceed the cap. Missing usage moves the reservation to an
  unreconciled amount that keeps counting against the cap.
- The configured cap is a local dispatch cap. It is not the provider's bill and does not
  guarantee it. The UI and summaries label it that way.
- Python `model_call` events are validated and aggregated into the same totals. The remaining
  budget is passed to the Python runner before its stage starts (`--remaining-budget-usd`), and
  Rust checks aggregate cost before starting the next stage.
- Secrets are redacted at event creation, so keys do not reach the log, the manifest, or the
  summary.

## 9. Source adapter status

| Adapter | Status |
|---------|--------|
| `fixture` (checked-in smoke inputs under `fixtures/smoke/`: one server, three versions, one task) | IMPLEMENTED, offline only |
| `mcpevol` (the supplied MCPEvol-Bench release) | IMPLEMENTED. Reads the seven files in section 1.1 read-only, exposes the labels `original` and `iter1` to `iter4`, the 201 inventory tasks with stage and change classes, and Tool KBs per label. Design and limits are in section 1.5. Checked offline against the invented miniature in `fixtures/mcpevol-mini/` and, when `MCP_DATA_DIR` and `EVAL_DATA_DIR` are set, against the real data (`crates/harness-core/tests/mcpevol_real.rs`). |

The full 201-task inventory is selectable in the harness. A live strategy run needs the
configured MCP server, an available model endpoint, and the owner's frozen settings (sections 3
and 6).

## 10. Output layout

A `harness run --output <dir>` writes:

| Path | Content |
|------|---------|
| `manifest.toml` | Resolved config without secrets, input checksums, contract versions |
| `events.jsonl` | Rust events for all strategies, merged with Python-source events in the order received |
| `python/<strategy>-rep<N>.jsonl` | Raw runner lines, verbatim |
| `summary.json` | Per strategy: adaptation status, canary status, verdict counts, metric events seen, cost totals including mock and unknown |
| `kb/` | Tool KB files handed to the runner |

Raw result artifacts intended for the project deliverable are not git-ignored.

## 11. Status against the plan's done criteria

| Done criterion | Status |
|----------------|--------|
| Protocol document names input checksums, actual counts, fixed task IDs, version stages, change classes, model and judge configuration, repetition count | Satisfied for checksums (1.1), actual counts (1.2), version stages (1.3), and change-class coverage (2.1). OPEN: fixed task IDs (a PROPOSED pilot is in 2.2; freezing waits for the owner's cost review). TO BE DECIDED BY PROJECT OWNER: model, decoding, judge version, repetitions, rate card. |
| TUI and headless CLI run the same core pipeline for the three strategies | Verify with the smoke command in the README. |
| Full supplied evaluation selectable; pilot covers Early/Middle/Late and Tool/Parameter/Description | The 201-task inventory is selectable through the `mcpevol` adapter. The proposed pilot covers every non-empty cell. Not fully satisfied as written: the Early change cells are empty by construction under the project's stage mapping (Early holds only controls), and Late x Parameter has one task. The frozen pilot is pending the owner's cost review. |
| Canary distinct from verifier; `not_applicable` for no update; failed or inconclusive canary does not promote | Rust logic and tests exist in `canary.rs`. Real canary use still needs isolated, resettable MCP server configuration. |
| Bridge validates and displays verifier events (three outcomes) | Fixture-backed. The Python process adapter does not implement a state-based completion verifier, so no live verifier events are available. |
| UI displays and preserves metric and under/over-update events | Fixture-backed. The process adapter emits task-level `tf`, `grounding`, and `pe`; live output is not yet verified. |
| Every Rust adaptation and canary call cost logged; Python cost events aggregated; remaining budget passed before a stage | Rust-side metering is implemented. The process adapter emits agent/judge usage for Rust ledger ingestion and applies a per-stage reservation cap; no live paid run has been made. |
| All verification commands exit 0; smoke makes no external request | See the README for the commands. |
| No supplied labels, judge, or server definitions modified; no secrets or source datasets committed | The adapter only reads the supplied files, and no benchmark content is in the repository. Judge version: pending. No secrets are stored by design. |
| Python process integration and live-run status | `benchmark/integration/rust_runner.py` implements the process contract. No end-to-end live run has been performed; target MCP servers and frozen settings are still required. |
| True evolved server definitions | Not available. The adapter reconstructs versions from a snapshot (sections 1.4 and 1.5). Real evolved definitions need the Hugging Face server bundles, which this project does not use. |
| Files outside scope unchanged; `plans/README.md` status updated | Owned by the orchestrating agent. |

## 12. Owner checklist before freezing this protocol

1. Review the proposed pilot in section 2.2, or change the rule's parameters in
   `PilotRule` and rerun the example. Decide how to treat the 7 tasks that touch unresolved servers
   or tools (section 2.3).
2. Prepare the matching target MCP server versions and run one task through the process adapter.
   Confirm the Rust Tool KB, task, model calls, tool events, and judge scores in the output.
3. Run the no-update pilot yourself (with the rate card filled in and a cap set) and review
   the cost.
4. Decide model, decoding, judge version, repetitions, cap, and rate card date; fill section 3;
   copy the values and the final task ids into a frozen config derived from
   `fixtures/mcpevol-real.example.toml`.
5. Record the SHA-256 of the frozen config: `shasum -a 256 path/to/frozen-config.toml`.
6. State in the report that `original`, `iter1`, and `iter2` are reconstructions and name the
   fidelity limits in section 1.5.

## 13. Commands to reproduce the numbers

Set the roots for your shell only. Do not write the path into a file.

```sh
export MCP_DATA_DIR=<benchmark clone>/src/custom/data
export EVAL_DATA_DIR=<benchmark clone>/src/custom/data
```

Confirm the checkout and the file hashes (section 1.1). Run these inside the data directory.

```sh
git -C "$MCP_DATA_DIR" rev-parse HEAD
(cd "$MCP_DATA_DIR" && shasum -a 256 test_cases/status_dict.json test_cases/test_cases.json \
  test_cases/chosen_idx.json mutation/HYBRID/evolution.json mutation/HYBRID/HYBRID/evolution.json \
  mutation/HYBRID/HYBRID/HYBRID/evolution.json mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json)
```

Counts, name resolution, unmapped items, and replay notes (sections 1.2 to 1.5). The example
config selects three task ids only to keep the output short; the counts printed for the whole
inventory do not depend on that selection.

```sh
cargo run -q -p harness-cli -- validate --config fixtures/mcpevol-real.example.toml
```

The coverage matrix (section 2.1), the proposed pilot (section 2.2), the `ids` line, and the
per-task table. The example leaves out tasks 299 and 401 (`EXCLUDE_FOR_CAUSE`) because their
external servers crash:

```sh
cargo run -q -p harness-core --example pilot_candidates
```

The example is read-only and deterministic. Two runs give identical output.

Version-level counts, per-version diffs, and the fidelity probe against the snapshot:

```sh
cargo test -q -p harness-core --test mcpevol_real -- --nocapture
```

Offline checks that need no benchmark data (invented miniature, mock provider):

```sh
cargo test --workspace --all-features
cargo run -q -p harness-cli -- validate --config configs/example.toml
```
