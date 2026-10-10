# Handoff: redesign as a coding-agent TUI (written 2026-10-04)

Read this first in a new session. It records the decisions and verified facts from the previous
session, so the next session can continue without that conversation.

## Where the repository stands

- Plan 001 (`plans/001-mcp-harness-tui.md`) produced a Rust workspace: `harness-core`,
  `harness-cli` (binary `harness`) and `harness-tui`. The last verified state was
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  and `cargo test --workspace --all-features` all exit 0, with 401 tests passing.
- Everything is untracked on `main`. Nothing has been committed or pushed. Ask before committing.
- `harness-core` holds the Tool KB types, schemas (`tool-kb/1`, `run-event/1`), version diffing,
  the three strategies (no update, full rewrite, selective patch), the canary gate, the cost
  ledger, the OpenAI/Gemini/local providers, and `McpEvolAdapter` for the real benchmark data.
- The current TUI is an experiment console (pick tasks, inspect diffs, start the strategies,
  read canary and score results). It is not an agent. The user rejected that design.
- `plans/README.md` marks plan 001 as BLOCKED (partial).

## Decisions the user made (authoritative)

1. Use everything in the MCPEvol-Bench repo as-is, because the TA asked for it. The repo's counts
   are authoritative; the brief's 123 servers / 1,272 tools appear only as an informational line.
2. Stage mapping: Early = `original` (cumulative evolution depth 0), Middle = `iter2` (depth 2),
   Late = `iter4` (depth 4). It is configurable in `[versions.stages]`.
3. The TUI must be a real agentic coding harness. Benchmarking is a separate command.
4. Benchmark our own agent. One Rust agent loop is shared by the TUI and the benchmark command.
   Person B's Python side only judges and scores our answer files with the benchmark's own scripts.
5. Tool descriptions come from the Tool KB, with a drift check. On connect, the agent compares each
   server's live `tools/list` with the KB, shows the drift in the TUI, and runs a strategy plus
   the canary before the KB changes. The model always sees tools as the KB describes them.
6. Built-in tools: the core coding set only (`read_file`, `list_dir`/glob, `grep`, `write_file`,
   `edit_file` as exact string replace, `run_shell`), scoped to the workspace, with approval for
   shell commands and file writes unless allowed for the session. Benchmark runs disable built-ins.
7. Approach A: build our own agent in Rust. codegraff (https://github.com/justrach/codegraff) is a
   design reference only. Copy no code from it.
8. Benchmark runs use live MCP servers through 1mcp: download the Hugging Face `evol-servers`
   bundles and run `original`, `iter3` and `iter5` locally, ideally in a container. Ask the user
   before every download or install.
9. The user wants `/improve` (plan mode) to write the detailed implementation plan for Person A
   (the Rust TUI/core owner) covering this redesign.

## Proposed structure (presented to the user, not yet written as a spec)

Three sub-projects, built in this order:

1. Agent core (new crate `harness-agent`): model clients with tool calling, streaming and
   reasoning effort for OpenAI and Gemini; an MCP client on `rmcp` (stdio and streamable HTTP);
   a tool registry where MCP tool descriptions come from the Tool KB; the built-in tools; an
   approval policy; a session transcript.
2. Coding TUI (`harness-tui` rewritten): chat view, tool-call cards, approval prompts, `/model`,
   `/effort`, `/mcp`, `/kb`, and a drift panel that runs a strategy plus canary.
3. Benchmark (`harness bench`, replacing `harness run`): runs MCPEvol tasks through the same agent
   loop against live 1mcp servers with built-ins off, and writes per-task answer files in the
   format the benchmark's `src/custom/analysis/answer/evaluation.py` reads (a directory whose path
   contains `ANSWER`, one `<idx>.json` per task with `trajectory_messages` and related fields).

`harness-core` stays. The experiment-console screens go. The proposed Python runner argv contract
in `docs/experiment-protocol.md` is replaced by "Person B judges our answer files", which needs
Person B's agreement.

## Models to support

The user named these; their exact API model IDs and prices are NOT known yet. Do not guess them.

| Provider | Model name as given | Reasoning effort |
|---|---|---|
| OpenAI | GPT 6 Luna | Max |
| OpenAI | GPT 6.1 Sol | xHigh |
| OpenAI | GPT 6 Astra | High |
| Google | Gemini 3.8 Flash | not specified |
| Google | Gemini 3.1 Pro | not specified |
| Google | Gemini 4 Argon | not released yet; add later |

Known gaps in the current code:

- No reasoning-effort setting anywhere. `ProviderSection` in `config.rs` has temperature, top_p,
  max_output_tokens and seed only.
- The OpenAI provider calls `POST {base}/chat/completions`. Check whether effort and tool calling
  for these models need the Responses API instead.
- The Gemini provider calls `models/{model}:generateContent` with no thinking setting. codegraff
  talks to Gemini through `https://generativelanguage.googleapis.com/v1beta/interactions`; check
  which endpoint the current docs recommend.
- One config runs one model. Five models means five configs or a small model-matrix feature.
- The cost ledger blocks paid calls until each model has a rate-card entry (price per million
  input and output tokens, with the price-page date).
- Budget: the project targets about USD 100. Max-effort reasoning tokens bill as output, so run a
  small pilot cost check first.

## First steps for the new session

1. Confirm the docs MCP tools are available. Expected servers and tools:
   - `openaiDeveloperDocs` (`https://developers.openai.com/mcp`): `search_openai_docs`,
     `fetch_openai_doc`, `list_openai_docs`, `list_api_endpoints`, `get_openapi_spec`.
   - `gemini-api-docs-mcp` (`https://gemini-api-docs-mcp.dev`): `gemini_search_docs`,
     `gemini_get_doc`.
   Both answered `initialize` and `tools/list` when probed with curl, but the previous
   conversation never loaded their tools. If they are still missing, tell the user and ask before
   using a workaround. `openaiDeveloperDocs` is registered at both user and local scope; that is
   redundant but harmless.
2. With those docs servers, find for each model: the exact API model ID, the reasoning-effort
   parameter and its allowed values, tool-calling request and response format, streaming format,
   usage fields (including reasoning tokens), and the current price per million tokens with the
   page date. Record a gap for anything the docs do not state.
3. Run `/improve` to write Person A's plan for the redesign, with those verified facts built in.
   Present a short design for each sub-project and get approval, per the brainstorming flow.

## Verified facts to carry forward

MCPEvol-Bench (https://github.com/Octobrist/MCPEvol-Bench, commit
`888c779a0fe735f612ca62bdd1fdd7c9e6bf8701`):

- Data lives in `src/custom/data`. `test_cases/chosen_idx.json` holds the 201 evaluation task
  indexes into `test_cases/test_cases.json` (393 cases). `test_cases/status_dict.json` has 163
  servers and 1,685 tools.
- `status_dict.json` is a live-server snapshot that already contains most depth 1 to 3 changes
  (parameter additions present: 72/84 at depth 1, 35/50 at depth 2, 96/114 at depth 3, 3/61 at
  depth 4). The adapter rewinds evidenced changes, so `original`, `iter1` and `iter2` are
  reconstructions.
- The four `mutation/HYBRID/.../evolution.json` files are cumulative. Each server entry has
  `mutation_type` (TOOL, DESC or PARAM) and `task_idx`, but only 11 of 347 `task_idx` values point
  at a task that uses the mutated server, so tasks are linked to changes with the benchmark
  runner's own involvement rule.
- `servers/1mcp_config/{original,iter3,iter5}/mcp.json` each list 167 servers. `original` runs 154
  through `npx` and 12 through `uvx`. `iter3` runs 69 servers and `iter5` runs 87 servers from the
  Hugging Face bundles (placeholders `/path/to/iter3-servers` and `/path/to/iter5-servers`). Some
  servers need extra services, for example MySQL.
- Whether `iter3`/`iter5` equal evolution depth 2/depth 4 is UNCONFIRMED. All four depths mutate the
  same 89 servers, so the server sets cannot tell them apart. Comparing a real bundle's
  `tools/list` with our replayed KBs would settle it.
- The benchmark's metrics are Task fulfillment, Planning effectiveness and the Evolutionary
  Competency Score (ECS). The judge is `src/custom/analysis/answer/evaluation.py`; ECS is
  `src/custom/analysis/answer/evolutionary_volatility.py`.
- The previous session's clone sat in a temporary directory that may be gone. Make a permanent
  clone (for example `~/Code-Projects/MCPEvol-Bench`, after asking the user) and point
  `MCP_DATA_DIR` and `EVAL_DATA_DIR` at its `src/custom/data`.

codegraff (commit `96809d6b8fc2e2d67224f48d150072af30a22886`):

- About 198,000 lines of Zig in 776 source files; needs Zig 0.17-dev, which is not installed here.
- Licence: AGPL-3.0 with extra terms. Its "funded companies" term (organisations that raised over
  USD 500,000) does not apply to a student project, so the AGPL applies to us. Copying code would
  make our project AGPL, which is why the decision is reference only.
- Useful design ideas: permission prompts, an effort command with levels up to `xhigh` and `Max`,
  paging of large tool outputs, a `mcp add` flow that checks a server before saving it.

Rust MCP SDK: crate `rmcp` 3.5.0, Apache-2.0, from `https://github.com/modelcontextprotocol/rust-sdk/`.
Check its current API in the docs before use.
