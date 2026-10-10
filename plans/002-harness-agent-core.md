# Plan 002: Build the `harness-agent` crate (model clients, MCP client, tools, agent loop)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update the status row for this plan in
> `plans/README.md`, unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: the repository had no commit when this plan was
> written, so `git rev-parse --short HEAD` may fail. Run
> `git status --short` and `git log --oneline -5` (if any commit exists, run
> `git diff --stat <first commit>..HEAD -- Cargo.toml crates/harness-core/src/cost.rs crates/harness-core/src/provider.rs`).
> Then confirm every excerpt under "Current state" still matches the live
> file. On a mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH (new async crate, two new provider APIs, child-process MCP servers, shell tool)
- **Depends on**: none (plan 001's Rust workspace must exist and pass its gates)
- **Category**: direction
- **Planned at**: no commit SHA (unborn `main`, all files untracked), 2026-10-04

## Why this matters

The current workspace is an experiment console, not an agent. The project owner decided
(2026-10-04) that the product must be a real agentic coding harness: one Rust agent loop that
talks to OpenAI and Gemini models with tool calling, connects to MCP servers, offers core coding
tools, asks for approval before risky actions, and is shared by the interactive TUI (plan 003)
and the benchmark command (plan 004). The existing providers cannot do this. Their
OpenAI provider calls Chat Completions, which has no tool calling for `gpt-6.1-sol` and
`gpt-6-astra`. Their Gemini provider calls `generateContent`, which Google now calls legacy.
This plan builds the shared agent core so the two later plans only add front ends.

## Current state

Workspace (`Cargo.toml`): members `crates/harness-core`, `crates/harness-tui`,
`crates/harness-cli`; edition 2024; `rust-version = "1.98"`; toolchain pinned to `1.98.0` in
`rust-toolchain.toml`; `[workspace.lints.rust] unsafe_code = "forbid"`. Every crate has
`[lints] workspace = true`. No async runtime anywhere; `harness-core` uses blocking `ureq 3.4.2`.

Relevant `harness-core` modules (read them before starting; do not rewrite them):

- `crates/harness-core/src/provider.rs`: sync `Provider` trait and `MeteredProvider`.
  ```rust
  // provider.rs:148-157
  pub trait Provider: Send {
      fn kind(&self) -> ProviderKind;
      fn id(&self) -> &str;
      fn model(&self) -> &str;
      fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;
  }
  ```
  `CompletionRequest` (provider.rs:42-60) has `purpose, system, user, max_output_tokens,
  temperature, top_p, seed, json_schema`. `UsageReport` (provider.rs:78-83) has
  `input_tokens` and `output_tokens` (both `Option<u64>`). `ProviderError` (provider.rs:100-132)
  has `MissingApiKey, Config, Http{status,message}, Timeout, Transport, InvalidResponse, Mock`
  and `billing_uncertain()`. `OpenAiProvider` posts to `{base}/chat/completions`;
  `GeminiProvider` posts to `models/{model}:generateContent`.
- `crates/harness-core/src/cost.rs`: `RateEntry` (cost.rs:36-45) has only
  `provider, model, input_usd_per_mtok, output_usd_per_mtok` with
  `#[serde(deny_unknown_fields)]`. `CostLedger::reserve(ReserveRequest) -> Result<Reservation, CostError>`
  (cost.rs:299) and `CostLedger::settle(Reservation, Usage) -> CostRecord` (cost.rs:343).
  `Usage` (cost.rs:164-173) has `request_id, input_tokens, output_tokens, latency_ms`.
- `crates/harness-core/src/model.rs`: `ToolCard { lineage_id, server_id, tool_id, name,
  server_version, description, input_schema, output_schema, usage_rules, provenance }` and
  `ToolKb { schema_version, kb_id, release_id, server_version, cards }` (`tool-kb/1`).
- `crates/harness-core/src/diff.rs:84`: `pub fn diff_kbs(source: &ToolKb, target: &ToolKb) -> KbDiff`.
- `crates/harness-core/src/strategy.rs:80-106`: `AdaptationContext { source, target, diff,
  provider: &mut MeteredProvider }`, `trait AdaptationStrategy`, `strategy_for(Strategy)`.
- `crates/harness-core/src/canary.rs`: `generate_probes(candidate, changed, control)` (canary.rs:537),
  `trait ProbeExecutor` (canary.rs:108), `SimulatedTargetExecutor`, `CanaryReport`.
- `crates/harness-core/src/redact.rs`: secret redaction used for logs. Reuse it for transcripts.

Conventions to match: module-level `//!` docs, `///` on every public item, `thiserror` error
enums with messages that never contain secrets (see `ProviderError`), API keys held in a
`Debug`-redacting newtype (see `ApiKey` at provider.rs:708-714), tests under `crates/<crate>/tests/`
with shared helpers in `tests/common/mod.rs` (see `crates/harness-tui/tests/common/mod.rs`).

### Verified external API facts (from `plans/model-api-facts-2026-10-04.md`, checked 2026-10-04)

Read that file in full before Step 4. The essentials:

| Alias | API model ID | Provider | Effort field | Allowed efforts | Default effort for this project |
|---|---|---|---|---|---|
| `luna` | `gpt-6-luna` | OpenAI | `reasoning.effort` | none, low, medium, high, xhigh, max | max |
| `sol` | `gpt-6.1-sol` | OpenAI | `reasoning.effort` | low, medium, high, xhigh, max | xhigh |
| `astra` | `gpt-6-astra` | OpenAI | `reasoning.effort` | low, medium, high, xhigh, max | high |
| `flash` | `gemini-3.8-flash` | Gemini | `generation_config.thinking_level` | low, medium, high | medium |
| `pro` | `gemini-3.1-pro-preview` | Gemini | `generation_config.thinking_level` | low, medium, high | high |

- OpenAI: `POST https://api.openai.com/v1/responses`, header `Authorization: Bearer <key>`.
  Body: `model`, `instructions`, `input` (array of items), `tools`, `reasoning: {"effort": ...}`,
  `max_output_tokens`, `stream`, `store: false`, `include: ["reasoning.encrypted_content"]`.
  Function tool: `{"type":"function","name","description","parameters","strict":false}`.
  Output items: `message` (content parts `output_text`), `function_call` (`id`, `call_id`,
  `name`, `arguments` as a JSON string), `reasoning`. Tool result input item:
  `{"type":"function_call_output","call_id","output":"<string>"}`. Pass back every output item
  since the last user message unchanged. Streaming events: `response.output_text.delta`
  (`delta`), `response.function_call_arguments.delta`, `response.completed` (full `response`
  with `output` and `usage`), `error`. Usage: `input_tokens`,
  `input_tokens_details.cached_tokens`, `input_tokens_details.cache_write_tokens`,
  `output_tokens` (includes reasoning), `output_tokens_details.reasoning_tokens`.
  `gpt-6-astra` rejects `temperature` and `top_p`. 429 `slow_down` and 503
  `server_is_overloaded` may carry `Retry-After`.
- Gemini: `POST https://generativelanguage.googleapis.com/v1beta/interactions`, header
  `x-goog-api-key: <key>`. Body: `model`, `input` (array of steps), `tools`, `store: false`,
  `generation_config: {"thinking_level": ...}`. Function tool:
  `{"type":"function","name","description","parameters"}`. Response: `status`
  (`requires_action` or `completed`), `steps` with types `thought`, `function_call`
  (`id`, `name`, `arguments` object), `model_output` (`content: [{type:"text",text}]`).
  Tool result step: `{"type":"function_result","name","call_id","result":[{"type":"text","text"}]}`.
  Stateless history must resend the `user_input` step, all model steps exactly as received,
  and the `function_result` steps. Usage: `total_input_tokens`, `total_cached_tokens`,
  `total_output_tokens`, `total_thought_tokens`; thought tokens bill at the output rate and are
  NOT included in `total_output_tokens`.
- Prices (USD per 1M tokens, Standard): see the facts file table; it includes cached-input,
  cache-write and long-context rates (OpenAI long context: more than 272K input tokens;
  Gemini 3.1 Pro: prompts over 200K tokens). Price date: 2026-10-04.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| All tests | `cargo test --workspace --all-features` | exit 0 |
| Agent tests only | `cargo test -p harness-agent --all-features` | exit 0 |
| Core tests only | `cargo test -p harness-core --all-features` | exit 0 |
| Dependency licences | `cargo tree -p harness-agent -e normal --prefix none \| sort -u` | lists crates; review for GPL/AGPL (none allowed) |

Before you change anything, run the three workspace gates once and record the passing test
count. The last recorded baseline was 401 passing tests.

## Suggested executor toolkit

- Read current docs for `rmcp` (docs.rs/rmcp, latest 3.x), `reqwest`, `tokio` and
  `tokio-util` before writing code against them; this plan names APIs that were current on
  2026-10-04 but you must confirm exact feature flags and signatures.
- Use the OpenAI and Gemini docs MCP servers (`openaiDeveloperDocs`, `gemini-api-docs-mcp`) if
  available, to re-check any field this plan marks as an assumption.

## Scope

**In scope**:
- `Cargo.toml` (workspace members and `[workspace.dependencies]` only)
- `crates/harness-agent/**` (create)
- `crates/harness-core/src/cost.rs`, `crates/harness-core/tests/cost.rs` (rate card and usage extension only)
- `crates/harness-core/src/config.rs`, `crates/harness-core/tests/config.rs` (only if needed to accept the extended `RateEntry`; no other config change)
- `configs/models.toml` (create)
- `schemas/agent-session.schema.json` (create)
- Any other file where the compiler forces the new `Usage`/`RateEntry` fields into existing
  struct literals (mechanical field additions only, no behaviour change; list each in the report)

**Out of scope** (do NOT touch):
- `crates/harness-tui/**` and `crates/harness-cli/**`: rewritten by plans 003 and 004.
- `crates/harness-core/src/provider.rs`: keep the existing providers working; they remain the
  path for `local` and `mock` and for existing tests. Do not change their behaviour.
- `crates/harness-core/src/{pipeline,runner,mcpevol,strategy,canary,diff,patch}.rs`: use their
  public APIs; do not modify them. If you need a change there, STOP.
- Anything that downloads MCP server bundles or installs `1mcp`, `npx` or `uvx` packages.
- Copying code from codegraff (https://github.com/justrach/codegraff). It is AGPL-3.0; it may be
  read as a design reference only.

## Git workflow

- Work on the current branch in the repository working tree. Do not create a branch or worktree.
- Do not commit unless the operator tells you to. If told to commit, use short imperative
  subjects (the repository has no commit history to copy) and no AI attribution lines.
- Do not push.

## Steps

### Step 1: Add the crate skeleton and dependencies

1. Add `crates/harness-agent` to `[workspace] members` and
   `harness-agent = { path = "crates/harness-agent" }` to `[workspace.dependencies]`.
2. Add these workspace dependencies (confirm each current version and licence first; all must be
   MIT, Apache-2.0, BSD or similar permissive):
   - `tokio` with features `rt-multi-thread, macros, sync, time, process, io-util, fs`
   - `tokio-util` (for `CancellationToken`)
   - `reqwest` with `default-features = false` and features `json, stream, rustls-tls`
   - `futures-util`
   - `rmcp` (latest 3.x) with the client features needed for: a child-process stdio
     transport, a streamable HTTP client transport, and `(AsyncRead, AsyncWrite)` transports.
     On 2026-10-04 the docs named `client`, `transport-child-process` and a
     `transport-streamable-http-client-*` feature. Use the exact names from docs.rs.
   - `globset` (glob matching for `list_dir`), `ignore` (gitignore-aware walking for `grep`)
   - `uuid` with `v4`
3. Create `crates/harness-agent/Cargo.toml` (copy the `[package]` workspace-inheritance block
   from `crates/harness-tui/Cargo.toml`; description "Agent loop, model clients, MCP client and
   tools for the MCP harness"), depending on `harness-core` plus the crates above,
   `serde`, `serde_json`, `thiserror`, `chrono`, `regex`. Dev-dependencies: `tempfile`, and
   `rmcp` with its `server` feature for in-process test servers.
4. Create `crates/harness-agent/src/lib.rs` with `//!` docs and empty modules:
   `model`, `registry`, `pricing`, `tools`, `mcp`, `approval`, `agent`, `events`, `transcript`,
   `drift`, `bridge`.

**Verify**: `cargo build -p harness-agent` → exit 0; `cargo tree -p harness-agent -i rmcp` → shows `rmcp 3.x`.

### Step 2: Extend the rate card and usage in `harness-core`

In `crates/harness-core/src/cost.rs`:

1. Add optional fields to `RateEntry` (all `#[serde(default, skip_serializing_if = "Option::is_none")]`
   so existing configs and fixtures stay valid):
   `cached_input_usd_per_mtok: Option<f64>`, `cache_write_usd_per_mtok: Option<f64>`,
   `long_context_threshold_tokens: Option<u64>`, `long_input_usd_per_mtok: Option<f64>`,
   `long_cached_input_usd_per_mtok: Option<f64>`, `long_cache_write_usd_per_mtok: Option<f64>`,
   `long_output_usd_per_mtok: Option<f64>`, `price_date: Option<String>` (ISO date).
2. Add to `Usage` (and every constructor in the crate and its tests):
   `cached_input_tokens: Option<u64>`, `cache_write_tokens: Option<u64>`,
   `reasoning_tokens: Option<u64>` (informational; already included in `output_tokens`).
3. Change `RateEntry::cost_usd` to take the whole `Usage`-like counts and compute:
   - `long = threshold.is_some_and(|t| input_tokens > t)`; pick the long or short rate set
     (fall back to short rates when a long rate is `None`).
   - `cached = cached_input_tokens.unwrap_or(0)`, `writes = cache_write_tokens.unwrap_or(0)`,
     `uncached = input_tokens.saturating_sub(cached + writes)`.
   - cost = `uncached * input + cached * cached_rate.unwrap_or(input) + writes * write_rate.unwrap_or(input) + output_tokens * output`, all divided by 1e6.
   - Assumption to record in a doc comment: `cached_tokens` and `cache_write_tokens` are
     subsets of `input_tokens` (the OpenAI caching guide computes hit rate as cached over input;
     cache writes are not stated). This only affects how cheap a cached call looks; it never
     under-reserves because reservations use the uncached input rate.
4. Reservation stays as today (input estimate at the input rate, `max_output_tokens` at the
   output rate), but uses the long-context rates when the estimate exceeds the threshold.

**Verify**: `cargo test -p harness-core --all-features` → exit 0. Add tests in
`crates/harness-core/tests/cost.rs` (pattern: the existing tests in that file):
(a) an old-format `RateEntry` TOML without new fields still parses; (b) cached tokens are billed
at the cached rate; (c) a request above the threshold uses long rates for the whole request;
(d) missing cached rate falls back to the input rate.

### Step 3: Model registry (`configs/models.toml`, `registry.rs`, `pricing.rs`)

1. Create `configs/models.toml` with one `[[models]]` table per row of the table above plus its
   full price set from `plans/model-api-facts-2026-10-04.md`. Fields:
   `alias, provider ("openai"|"gemini"), model_id, api_key_env ("OPENAI_API_KEY"|"GEMINI_API_KEY"),
   allowed_efforts, default_effort, accepts_sampling (false for gpt-6-astra; false for every
   model unless the docs say otherwise, so the client never sends temperature/top_p),
   max_output_tokens (OpenAI 128000, Gemini 65536), and a `[models.price]` sub-table mirroring
   the extended `RateEntry` (price_date = "2026-10-04")`. Add a header comment pointing at the
   facts file. Do not add Gemini 4 Argon (no verified ID or price).
2. `registry.rs`: `Effort` enum (`None, Minimal, Low, Medium, High, Xhigh, Max`, serde
   lowercase), `ProviderId` enum (`OpenAi, Gemini`), `ModelSpec`, `ModelRegistry::load(path)`,
   `get(alias)`, `validate()` (default effort must be in allowed set; aliases unique; price
   present), and `ModelSpec::effort_supported(Effort) -> bool`.
3. `pricing.rs`: `fn rate_entry(spec: &ModelSpec) -> harness_core::cost::RateEntry` and
   `fn rate_card(registry) -> RateCard` so the agent can reserve and settle with the core ledger.

**Verify**: `cargo test -p harness-agent registry` → passes new tests: loads
`configs/models.toml`; all five aliases present; `sol` rejects `none`; `flash` rejects
`minimal`; `astra.accepts_sampling == false`; an invalid default effort fails validation.

### Step 4: Provider-neutral conversation types and the `ModelClient` trait (`model/mod.rs`)

Define:

```rust
pub enum Item {
    User { text: String },
    Assistant(AssistantTurn),
    ToolResult { call_id: String, name: String, output: String, is_error: bool },
}

pub struct AssistantTurn {
    pub model_alias: String,          // which model produced it
    pub text: String,                 // concatenated visible text
    pub tool_calls: Vec<ToolCall>,
    pub raw: Vec<serde_json::Value>,  // provider output items / steps exactly as received
}

pub struct ToolCall { pub call_id: String, pub name: String, pub arguments: serde_json::Value, pub raw_arguments: String }

pub struct ModelRequest<'a> {
    pub spec: &'a ModelSpec,
    pub effort: Effort,
    pub instructions: &'a str,
    pub history: &'a [Item],
    pub tools: &'a [ToolSchema],      // name, description, parameters (JSON Schema)
    pub max_output_tokens: u32,
    pub json_schema: Option<&'a serde_json::Value>,
}

pub struct ModelResponse { pub turn: AssistantTurn, pub usage: TokenUsage, pub request_id: String, pub latency_ms: u64 }
pub struct TokenUsage { pub input: Option<u64>, pub cached_input: Option<u64>, pub cache_write: Option<u64>, pub output_including_reasoning: Option<u64>, pub reasoning: Option<u64> }

// Plain trait returning a boxed future (do NOT add the async-trait crate).
pub trait ModelClient: Send + Sync {
    fn provider(&self) -> ProviderId;
    fn send<'a>(&'a self, req: ModelRequest<'a>, stream: tokio::sync::mpsc::UnboundedSender<StreamEvent>, cancel: tokio_util::sync::CancellationToken)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ModelResponse, ModelError>> + Send + 'a>>;
}
pub enum StreamEvent { TextDelta(String), ToolCallStarted { call_id: String, name: String } }
```

`ModelError` mirrors `ProviderError` (same variants plus `Cancelled` and
`UnsupportedEffort`), with a `billing_uncertain()` that matches core's rule.

History replay rule (implement as a shared helper and test it): an `AssistantTurn` is replayed
from `raw` only when `turn.model_alias == req.spec.alias`. Otherwise it is "foreign" and is
rendered from `text` and `tool_calls` (see each client below). This keeps encrypted reasoning
and thought signatures with the model that produced them.

**Verify**: `cargo build -p harness-agent` → exit 0.

### Step 5: SSE reader and OpenAI Responses client (`model/sse.rs`, `model/openai.rs`)

1. `sse.rs`: incremental parser over a `reqwest` byte stream: accumulate lines; `event:` and
   `data:` fields; an empty line ends an event; ignore lines starting with `:`; a `data: [DONE]`
   ends the stream. Unit-test with chunk boundaries in the middle of lines.
2. `openai.rs`: `OpenAiClient { http: reqwest::Client, base_url, api_key: ApiKey }`. Base URL
   default `https://api.openai.com/v1`, overridable for tests; refuse non-HTTPS unless the host
   is loopback (same rule as core's `require_https`). Request mapping:
   - `instructions` from the request; `store: false`; `include: ["reasoning.encrypted_content"]`;
     `stream: true`; `reasoning: {"effort": <effort>}`; `max_output_tokens`.
   - Never send `temperature`, `top_p` or `seed` when `accepts_sampling == false`.
   - Tools: `{"type":"function","name","description","parameters","strict":false}`.
   - History: `User` → `{"role":"user","content":text}`; own `AssistantTurn` → each `raw` item
     verbatim; foreign `AssistantTurn` → `{"role":"assistant","content":text}` (omit when text is
     empty) followed by one `{"type":"function_call","call_id","name","arguments":raw_arguments}`
     per tool call; `ToolResult` → `{"type":"function_call_output","call_id","output"}`.
   - `json_schema` (only used by the core bridge) → `text: {"format": {"type":"json_schema", ...}}`.
     Confirm the exact `text.format` shape in the Responses structured-outputs docs first; if you
     cannot confirm it, put the schema in the instructions instead and note it in the report.
3. Response handling: forward `response.output_text.delta` as `StreamEvent::TextDelta`;
   on `response.output_item.added` with a `function_call` item forward `ToolCallStarted`;
   take `response.completed.response` as authoritative: `raw = output` items, `text` = all
   `output_text` parts joined, `tool_calls` = every `function_call` item (parse `arguments`; if
   parsing fails keep `Value::String(raw)` and let the tool layer report the error), usage from
   `usage`. An `error` event or a `response.failed`/`response.incomplete` status maps to
   `ModelError::InvalidResponse` with the server message (truncate to 500 chars, redact).
4. Retries: on 429 or 503, wait `Retry-After` seconds if present, else 1s, 2s, 4s; at most 4
   attempts; respect the cancellation token while waiting.

**Verify**: `cargo test -p harness-agent openai` → new tests pass. Use a local HTTP test server
(`tokio::net::TcpListener` serving canned SSE text, base URL `http://127.0.0.1:<port>`) to cover:
text-only reply; reply with two function calls; reasoning item replayed verbatim on the next
request (assert the captured request body); astra request has no `temperature`; 429 with
`Retry-After: 0` then success; `strict:false` on every tool.

### Step 6: Gemini Interactions client (`model/gemini.rs`)

`GeminiClient` with base URL default `https://generativelanguage.googleapis.com/v1beta`.
Use **non-streaming** requests (`stream: false`) in this plan: the streamed
`interaction.completed` event does not include `steps`, and rebuilding steps from deltas risks
breaking the "exactly as received" replay rule. Request mapping:

- `model`, `store: false`, `generation_config: {"thinking_level": <effort>}` (only low, medium,
  high are valid; reject others with `UnsupportedEffort` before sending).
- `instructions`: confirm in the Interactions API reference whether a system-instruction field
  exists. If it does, use it. If not, prepend the instructions as the first `user_input` step and
  note this in your report.
- Tools: `{"type":"function","name","description","parameters"}`.
- History: `User` → `{"type":"user_input","content":text}`; own `AssistantTurn` → every `raw`
  step verbatim; `ToolResult` → `{"type":"function_result","name","call_id","result":[{"type":"text","text":output}]}`.
  Foreign `AssistantTurn`: Gemini function-call steps without thought signatures may be rejected,
  so render foreign turns as a `user_input` step whose content is a plain transcript
  ("Assistant said: ...", "Assistant called <name> with <args>", "Tool <name> returned: ...")
  and drop the matching foreign `ToolResult` items from structured form.
- Response: `raw = steps`; `text` from `model_output` text parts; `tool_calls` from
  `function_call` steps (`call_id = id`, `arguments` object, `raw_arguments = arguments.to_string()`);
  emit the full text as one `TextDelta` at the end.
- Usage: `input = total_input_tokens`, `cached_input = total_cached_tokens`,
  `output_including_reasoning = total_output_tokens + total_thought_tokens`,
  `reasoning = total_thought_tokens`.
- Same retry and HTTPS rules as Step 5.

**Verify**: `cargo test -p harness-agent gemini` → tests pass for: text reply; function call
with `requires_action`; next request replays the `thought` and `function_call` steps verbatim
and appends `function_result`; `thinking_level` sent; `minimal` rejected before any HTTP call;
usage adds thought tokens to output.

### Step 7: Mock model client (`model/mock.rs`)

`MockModelClient` with a scripted queue of `ModelResponse`s (or errors), recording every
`ModelRequest` it receives (clone the history and tool names). Loadable from a JSON fixture
directory, mirroring `MockProvider::from_fixture_dir` in core. Used by every later test and by
plans 003 and 004.

**Verify**: `cargo test -p harness-agent mock` → passes.

### Step 8: MCP client and tool registry (`mcp.rs`, `tools/mod.rs`)

1. `mcp.rs`: `McpServerConfig` that deserialises the standard `mcp.json` shape used by the
   benchmark's 1mcp configs: `{"mcpServers": {"<name>": {"command", "args", "env"}}}` plus an
   optional `"url"` for streamable HTTP. `McpConnection` wraps an `rmcp` client service:
   `connect_stdio(cfg)`, `connect_http(url)`, `list_tools() -> Vec<LiveTool{name, description, input_schema}>`,
   `call_tool(name, args) -> CallOutcome{text, is_error}` (join text content parts; describe
   non-text parts as `[<type> content omitted]`), `shutdown()` (graceful, then kill). Child stderr
   goes to a per-server log file under the session directory, never to the terminal.
   Per-call timeout (default 120 s, configurable).
2. `tools/mod.rs`: `ToolSpec { exposed_name, description, parameters, source: ToolSource }`,
   `ToolSource::{Builtin(BuiltinKind), Mcp { server: String, tool: String }}`, and
   `ToolRegistry` with:
   - `NamingMode::Namespaced` (TUI): `mcp__<server>__<tool>`, characters outside
     `[A-Za-z0-9_-]` replaced by `_`, truncated to 64 characters with a 6-character hash suffix
     when longer; collisions are an error.
   - `NamingMode::Bare` (benchmark): the tool name as-is; a collision across servers is an error.
   - `add_mcp_from_kb(server, kb: &ToolKb, live: &[LiveTool])`: for each KB card with
     `server_id == server`, expose it with the **KB** description and `input_schema`. Live tools
     never change what the model sees. Live tools missing from the KB are not exposed.
   - `to_schemas() -> Vec<ToolSchema>` for `ModelRequest`.
3. Tests use an in-process rmcp server over `tokio::io::duplex` (dev-dependency `rmcp` with
   `server`) exposing two tools; assert `list_tools`, `call_tool`, and that the registry exposes
   the KB description even when the live description differs.

**Verify**: `cargo test -p harness-agent mcp` and `cargo test -p harness-agent tools::` → pass.

### Step 9: Built-in coding tools (`tools/builtin.rs`)

All paths are relative to a canonicalised workspace root. Resolution: reject absolute paths
outside the root; join; for existing paths `canonicalize` and require `starts_with(root)`; for
new files canonicalise the nearest existing ancestor and require the same (this blocks `..` and
symlink escapes). Tools and schemas:

| Name | Arguments | Behaviour | Approval class |
|---|---|---|---|
| `read_file` | `path`, optional `offset` (1-based line), `limit` (lines, default 2000) | returns numbered lines; refuses files over 10 MiB; marks truncation | Read |
| `list_dir` | `path` (default "."), optional `glob` | entries (dirs end with `/`), skips `.git`, max 1000, globset match | Read |
| `grep` | `pattern` (regex), optional `path`, `glob` | `ignore::WalkBuilder` (respects .gitignore), `file:line: text`, max 200 matches | Read |
| `write_file` | `path`, `content` | create or overwrite; creates parent dirs inside root | Write |
| `edit_file` | `path`, `old_string`, `new_string`, optional `replace_all` | exact string replace; error if `old_string` occurs 0 times, or more than once without `replace_all` | Write |
| `run_shell` | `command`, optional `timeout_secs` (default 120, max 600) | `sh -c` in the root; env inherited minus every variable named in the model registry's `api_key_env` fields; stdout+stderr captured, each capped at 30,000 bytes with a `[truncated N bytes]` note; exit code reported | Shell |

Every tool returns a `ToolOutput { text, is_error }`; failures are tool errors fed back to the
model, never panics.

**Verify**: `cargo test -p harness-agent builtin` → tests with `tempfile` cover: read with
offset/limit; `../outside` rejected; symlink pointing outside rejected; edit with 0 and 2
matches fails; `replace_all` works; shell timeout kills the process; shell env lacks
`OPENAI_API_KEY` even when the test sets it.

### Step 10: Approval policy (`approval.rs`)

`ApprovalClass::{Read, Write, Shell, Mcp}`; `ApprovalPolicy` with per-class default
(`Read`=Allow, `Write`=Ask, `Shell`=Ask, `Mcp`=Allow) and a session allow-set. `Decision::{Once,
Session, Deny}`. `Session` adds the class to the allow-set. Interface used by the agent loop:

```rust
pub struct ApprovalRequest { pub id: u64, pub tool: String, pub class: ApprovalClass, pub summary: String, pub diff: Option<String> }
// The loop sends AgentEvent::ApprovalRequested(req, oneshot::Sender<Decision>) and awaits it.
```

For `write_file` and `edit_file`, `diff` is a unified diff of the proposed change (simple
line-based diff is enough; reuse `crates/harness-tui/src/diffview.rs` logic only by reading it,
do not depend on harness-tui). A denied call returns the tool error text
"The user denied this tool call." to the model.

**Verify**: `cargo test -p harness-agent approval` → passes: default asks for write; `Session`
stops later prompts for that class; `Deny` yields the denial text.

### Step 11: Agent loop, events and transcript (`agent.rs`, `events.rs`, `transcript.rs`)

1. `events.rs`: `AgentEvent` enum: `TurnStarted`, `TextDelta(String)`,
   `ToolCallStarted{call_id,name,arguments}`, `ToolCallFinished{call_id,output,is_error,duration_ms}`,
   `ApprovalRequested(ApprovalRequest, oneshot::Sender<Decision>)`, `Usage{model_alias, usage, cost_usd}`,
   `Drift(DriftReport)`, `Notice(String)`, `Error(String)`, `TurnFinished{reason: StopReason}`.
2. `agent.rs`: `AgentConfig { model_alias, effort, instructions, max_steps, tool_result_max_chars: Option<usize>, builtins_enabled, stop_policy: StopPolicy }`.
   `StopPolicy::Interactive` (stop when an assistant turn has no tool calls) and
   `StopPolicy::Benchmark { finish_marker: String, nudge_text: String, max_no_tool_turns: u32 }`
   (stop when the text contains the marker; when a turn has text and no tool calls, append a
   user message with `nudge_text` until `max_no_tool_turns` such turns have happened, then
   stop; a turn with neither text nor tool calls stops). Plan 004 supplies the benchmark values;
   implement and unit-test the mechanics here.
   `Agent::run_turn(user_text, events: UnboundedSender<AgentEvent>, cancel)`: append `User`;
   loop up to `max_steps` model calls: reserve budget (core `CostLedger` behind
   `Arc<Mutex<_>>`, with `harness_core::events::Phase::Agent` for agent calls and
   `Phase::Adaptation` for bridge calls made by strategies), call the client, settle usage, append the `AssistantTurn`, execute its tool calls
   **sequentially** (approval first when the class requires it), append `ToolResult`s with
   output truncated to `tool_result_max_chars` when set. Budget refusal ends the turn with
   `StopReason::Budget`. Cancellation ends it with `StopReason::Cancelled` and appends no
   partial assistant turn.
   `Agent::set_model(alias, effort)` validates against the registry; allowed only between turns.
3. `transcript.rs`: append-only JSONL at `<session_dir>/transcript.jsonl`, one object per line:
   `{"schema":"agent-session/1","ts":<RFC3339>,"kind":<event kind>,...}`. Run every string
   through `harness_core::redact` before writing. Write `schemas/agent-session.schema.json`
   describing the line shape and validate written lines against it in a test (core already uses
   the `jsonschema` crate; follow `crates/harness-core/tests/contracts.rs`).

**Verify**: `cargo test -p harness-agent agent` → tests with `MockModelClient` and the in-process
MCP server cover: two-step tool loop ending with text; write tool waits for approval and
proceeds on `Once`; denial path; `max_steps` reached → `StopReason::MaxSteps`; benchmark policy
stops on `[FINISH]`; nudge appended twice then stop on the third no-tool turn; budget cap
refusal; transcript lines validate against the schema and contain no test API key string.

### Step 12: Drift check (`drift.rs`)

`DriftReport { server, kb_cards, live_tools, diff: KbDiff }` built by converting the live
`tools/list` into a temporary `ToolKb` (same `server_id`, `server_version = "live"`,
`lineage_id` = KB lineage when the name matches, else `"live:<name>"`) and calling
`harness_core::diff::diff_kbs(kb_subset, live_kb)`. `run_adaptation(strategy, kb, live_kb, diff, provider)`
runs `harness_core::strategy::strategy_for(strategy).adapt(...)` and then
`generate_probes` + `SimulatedTargetExecutor` to produce a `CanaryReport`. The session KB
changes only through an explicit `accept(candidate)` call; nothing is accepted automatically.
Read `crates/harness-core/src/pipeline.rs` to see how plan 001 wires strategy and canary
together and copy that call sequence (not its file I/O).

**Verify**: `cargo test -p harness-agent drift` → a KB whose card description differs from the
live tool yields a non-empty diff; `NoUpdate` returns the original KB; accepting a candidate
changes what `ToolRegistry::add_mcp_from_kb` exposes.

### Step 13: Core `Provider` bridge (`bridge.rs`)

`BlockingProvider { client: Arc<dyn ModelClient>, spec: ModelSpec, effort: Effort, handle: tokio::runtime::Handle }`
implementing `harness_core::provider::Provider`. `complete()` builds a `ModelRequest` with
`instructions = req.system`, one `User` item with `req.user`, no tools, `json_schema` passed
through, and calls `handle.block_on(...)`. Document that it must be called from a thread that is
not a runtime worker (use `tokio::task::spawn_blocking`). Map usage to `UsageReport`
(`output_tokens` = output including reasoning). `kind()` returns `ProviderKind::Paid` for both
providers. This lets the drift flow run core strategies through `MeteredProvider` with the new
clients.

**Verify**: `cargo test -p harness-agent bridge` → a test inside `spawn_blocking` runs
`FullRewrite` through `MeteredProvider<BlockingProvider<MockModelClient>>` and the ledger records
one settled call.

### Step 14: Full gates

**Verify**: `cargo fmt --all -- --check` → exit 0;
`cargo clippy --workspace --all-targets --all-features -- -D warnings` → exit 0;
`cargo test --workspace --all-features` → exit 0 with at least the baseline count plus the new
tests; `git status --short` shows only in-scope paths changed.

## Test plan

New test files under `crates/harness-agent/tests/`: `registry.rs`, `sse.rs`, `openai.rs`,
`gemini.rs`, `mcp.rs`, `builtin.rs`, `approval.rs`, `agent.rs`, `drift.rs`, `bridge.rs`,
`transcript.rs`, with helpers (local SSE/HTTP stub server, in-process MCP server, mock scripts)
in `tests/common/mod.rs`. Model the structure on `crates/harness-core/tests/provider.rs` and
`crates/harness-tui/tests/common/mod.rs`. No test may contact a real provider or MCP server, and
none may read a real API key; tests set dummy env values and assert they never appear in output.
Core additions go in `crates/harness-core/tests/cost.rs`.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` exits 0
- [ ] `cargo test --workspace --all-features` exits 0; new `harness-agent` test files exist and pass
- [ ] `configs/models.toml` contains exactly the aliases `luna, sol, astra, flash, pro`
- [ ] `grep -rn "chat/completions\|:generateContent" crates/harness-agent/src` returns no matches
- [ ] `grep -rn "\"strict\": *true\|strict: true" crates/harness-agent/src` returns no matches
- [ ] `cargo tree -p harness-agent -e normal --prefix none | grep -i -E "agpl|gpl-3"` returns nothing (and no dependency licence is GPL/AGPL)
- [ ] `git status --short` shows changes only under the in-scope paths
- [ ] `plans/README.md` row for 002 updated

## STOP conditions

Stop and report back (do not improvise) if:

- `rmcp` 3.x has no client transport for child processes, streamable HTTP, or duplex streams,
  or its client API differs so much that Step 8 cannot be written as described.
- The OpenAI Responses or Gemini Interactions docs contradict a field in "Verified external API
  facts" (for example, `store:false` replay requires something else, or `strict:false` is
  rejected). Report the doc text and URL.
- Gemini rejects replayed steps in tests you write against the documented shape, or the
  Interactions API has no way to pass instructions and prepending them breaks function calling.
- A change to an out-of-scope `harness-core` module seems necessary (for example a strategy
  needs a different provider signature).
- A required dependency has a GPL or AGPL licence.
- The workspace gates fail before you start (baseline broken).

## Maintenance notes

- The model table and prices are dated 2026-10-04. Gemini 3.8 Flash prices double on
  2027-01-01; Gemini 3.1 Pro is a preview model and may be renamed. Update `configs/models.toml`
  and the facts file together.
- When Gemini 4 Argon ships, add a registry entry only after checking its ID, thinking levels and
  price in the docs.
- Gemini streaming is deliberately off. Revisit only if the docs show how to get complete steps
  (with signatures) from a stream.
- Reviewers should scrutinise: path resolution in `builtin.rs`, the shell env scrub, that KB
  descriptions (not live ones) reach the model, and that cost settlement adds Gemini thought
  tokens.
- Deferred: parallel tool execution, context compaction, `gemini-3.1-pro-preview-customtools`
  evaluation, a live-probe canary executor (live probes can mutate server state; the simulated
  executor is used until isolation is designed).
