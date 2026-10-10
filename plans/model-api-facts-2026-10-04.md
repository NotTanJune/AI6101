# Model API facts (verified 2026-10-04)

Source: the `openaiDeveloperDocs` and `gemini-api-docs-mcp` docs servers, queried on 2026-10-04.
Neither pricing page shows a "last updated" date, so the fetch date (2026-10-04) is the price date
for the cost ledger rate card. Anything the docs did not state is listed under "Gaps".

## Model IDs and reasoning settings

| Name given by user | API model ID | Provider status | Effort parameter | Allowed values | Target effort |
|---|---|---|---|---|---|
| GPT 6 Luna | `gpt-6-luna` | GA | `reasoning.effort` | `none`, `low`, `medium` (default), `high`, `xhigh`, `max` | `max` |
| GPT 6.1 Sol | `gpt-6.1-sol` | GA | `reasoning.effort` | `low`, `medium` (default), `high`, `xhigh`, `max` (`none`, `minimal` rejected) | `xhigh` |
| GPT 6 Astra | `gpt-6-astra` | GA | `reasoning.effort` | `low`, `medium`, `high`, `xhigh`, `max` (no `none`) | `high` |
| Gemini 3.8 Flash | `gemini-3.8-flash` | Stable (Sept 2026) | `generation_config.thinking_level` | `low`, `medium` (default), `high` (`minimal` returns an error) | not specified |
| Gemini 3.1 Pro | `gemini-3.1-pro-preview` | Preview (Feb 2026) | `generation_config.thinking_level` | `low`, `medium`, `high` (default) (`minimal` not supported) | not specified |
| Gemini 4 Argon | none | Not in the docs | n/a | n/a | add later |

Notes:

- A separate `gpt-6-sol` model also exists. The user named "GPT 6.1 Sol", so the ID is `gpt-6.1-sol`.
- `gemini-3.1-pro-preview-customtools` is a documented variant that "is better at prioritizing your
  custom tools". It suits an agent with built-in tools, but it is a second model ID to evaluate.
- GPT-6 Astra does not accept custom `temperature`, `top_p` or `logprobs` (API changelog,
  September 2026). The existing `temperature`/`top_p` config fields must be omitted for it.
- All three OpenAI models: context 1,050,000 tokens, max input 922,000, max output 128,000.
  Both Gemini models: input limit 1,048,576, output limit 65,536.

## OpenAI: Responses API is required

- Tool calling needs the Responses API (`POST https://api.openai.com/v1/responses`).
  `gpt-6.1-sol` and `gpt-6-astra`: Chat Completions has no tool calling.
  `gpt-6-luna`: Chat Completions supports function calling only with `reasoning_effort: none`.
  The current provider (`POST {base}/chat/completions`) cannot run the agent for any of the three.
- Request: `model`, `input` (array of items), `instructions`, `tools`, `tool_choice`,
  `reasoning: {"effort": "..."}`, `max_output_tokens` (covers visible output plus reasoning),
  `stream`, `store`, `parallel_tool_calls`, `include`.
- Function tool: `{"type":"function","name":...,"description":...,"parameters":{JSON Schema},"strict":...}`.
  If `strict` is omitted, Responses tries to normalise the schema into strict mode and falls back
  to non-strict. Strict mode requires `additionalProperties: false` and every property in
  `required`. MCP tool schemas will often fail that, so send `"strict": false` explicitly so the
  model sees the Tool KB schema unchanged.
- Response `output` items: `message` (content `output_text`), `function_call`
  (`id`, `call_id`, `name`, `arguments` as a JSON string), and reasoning items.
- Tool result input item: `{"type":"function_call_output","call_id":...,"output":"<string>"}`.
- Reasoning items: pass back all reasoning, function call and function call output items since the
  last user message, either through `previous_response_id` or by appending the prior `output`
  items to `input`. For stateless use (`store: false`), add
  `include: ["reasoning.encrypted_content"]` so reasoning items can be replayed.
- Streaming: typed SSE events. Text: `response.created`, `response.output_item.added`,
  `response.output_text.delta`, `response.output_text.done`, `response.output_item.done`,
  `response.completed`, `error`. Function calls: `response.function_call_arguments.delta`,
  `response.function_call_arguments.done`.
- Usage: `usage.input_tokens`, `usage.input_tokens_details.cached_tokens`,
  `usage.input_tokens_details.cache_write_tokens`, `usage.output_tokens`,
  `usage.output_tokens_details.reasoning_tokens`, `usage.total_tokens`.
  Reasoning tokens are billed as output tokens and are included in `output_tokens`
  (example: `output_tokens` 1035 with `reasoning_tokens` 832).
- Errors: 429 with code `slow_down`, 503 with `server_is_overloaded`; honour `Retry-After`,
  otherwise exponential backoff.

## Gemini: Interactions API is the default

- "The Interactions API has become our default interface as of June 2026 ... the
  `generateContent` API is now considered legacy." The current provider uses
  `models/{model}:generateContent`; the redesign should target
  `POST https://generativelanguage.googleapis.com/v1beta/interactions` (header `x-goog-api-key`).
- Request: `model`, `input` (string or array of steps), `tools`, `store`, `stream`,
  `generation_config: {"thinking_level": "..."}`.
- Function tool: `{"type":"function","name":...,"description":...,"parameters":{JSON Schema}}`.
- Response: `status` (`requires_action` when a function call is pending, `completed`), `steps`
  array. Step types: `thought` (carries `thought_signature`), `function_call` (`id`, `name`,
  `arguments` as an object), `model_output` (content `text`).
- Tool result step: `{"type":"function_result","name":...,"call_id":<function_call id>,"result":[{"type":"text","text":...}]}`.
- Stateless mode (`store: false`): each request must resend the initial `user_input`, every
  model step "exactly as received" (including `thought` steps with signatures), and the
  `function_result` steps.
- Streaming (`stream: true`, SSE): `interaction.created`, `interaction.status_update`,
  `step.start` (step type; for `function_call` it includes `id`, `name`, empty `arguments`),
  `step.delta` (delta types `text`, `thought_signature`, `thought_summary`, `arguments_delta`;
  arguments arrive as partial JSON strings and must be accumulated), `step.stop`,
  `interaction.completed` (final `usage`), `error`, then `done` with `[DONE]`.
- Usage: `total_input_tokens`, `total_cached_tokens`, `total_output_tokens`,
  `total_thought_tokens`, `total_tool_use_tokens`, `total_tokens`. Thinking tokens are billed at
  the output rate. In the streaming example `total_tokens` (346) = input 11 + output 90 +
  thought 245, so `total_output_tokens` excludes thought tokens; the ledger must add them.

## Prices (USD per 1M tokens, Standard tier, prompts at or under the long-context threshold)

| Model ID | Input | Cached input | Cache write | Output (incl. reasoning) | Long-context rule |
|---|---|---|---|---|---|
| `gpt-6-astra` | 10.00 | 1.00 | 12.50 | 50.00 | >272K input: 20.00 / 2.00 / 25.00 / 75.00 |
| `gpt-6.1-sol` | 2.00 | 0.10 | 2.50 | 10.00 | >272K input: 4.00 / 0.20 / 5.00 / 15.00 |
| `gpt-6-luna` | 0.10 | 0.01 | 0.125 | 0.50 | >272K input: 0.20 / 0.02 / 0.25 / 0.75 |
| `gemini-3.8-flash` | 0.75 | 0.075 | n/a (storage 0.50 per 1M tokens per hour) | 3.75 | Promo until 2026-12-31; from 2027-01-01: 1.50 / 0.15 / 7.50 |
| `gemini-3.1-pro-preview` | 2.00 | 0.20 | n/a (storage 4.50 per 1M tokens per hour) | 12.00 | >200K prompt: 4.00 / 0.40 / 18.00 |

Batch and Flex are 50% of Standard for all five models. The benchmark runs interactively through
an agent loop, so Standard is the planning rate.

## Rough cost scale (illustrative, not a measurement)

Assumption: one benchmark task costs 20K input and 5K output tokens (including reasoning), with
no caching. 201 tasks x 3 stages = 603 runs per model.

| Model | Cost per run | 603 runs |
|---|---|---|
| `gpt-6-astra` | 0.45 | about 271 |
| `gpt-6.1-sol` | 0.09 | about 54 |
| `gemini-3.1-pro-preview` | 0.10 | about 60 |
| `gemini-3.8-flash` | 0.034 | about 20 |
| `gpt-6-luna` | 0.0045 | about 3 |

Total about USD 409 against a USD 100 target, before multi-turn resends (an agent loop resends
the growing history each turn, so real input is larger; caching offsets part of that). A pilot on
a few tasks per model is required before any full run, and the task count, stage count or model
set likely needs to shrink.

## Gaps

- Tool definitions per task are small: the benchmark runner
  (`src/custom/real_mcp/tool_level_test.py`, `return_involved_tool_status`) exposes only the
  tools a task lists. Across the 393 cases in `test_cases.json` that is 3.97 tools on average,
  9 at most. Tool results are cut to 500 characters, and rounds are capped at
  `max(10, 2 x tools)`. The 20K-input assumption above is plausible but unmeasured.
- The benchmark re-runs only mutation-involved tasks at evolved stages and reuses the base
  answer for the rest (`return_involved_mutated_test_cases`, `count_mutation_scores`), so
  Middle and Late cost less than a full 201-task pass.
- Real per-task token usage at `max`/`xhigh` effort is unknown until a pilot runs.
- Gemini 3.1 Pro and 3.8 Flash: the user gave no thinking level. Defaults are `high` (Pro) and
  `medium` (Flash).
- Whether `gemini-3.1-pro-preview-customtools` should replace the base Pro ID is undecided.
- Gemini 4 Argon has no model page, ID or price.
- The OpenAI docs do not say whether `max` effort has its own output-token cap below 128,000.
