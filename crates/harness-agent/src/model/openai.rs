//! OpenAI Responses API client (`POST {base}/responses`, streaming).
//!
//! Requests are stateless (`store: false`) and ask for `reasoning.encrypted_content`, so the
//! reasoning items of a turn can be replayed verbatim on the next request. Function tools are
//! always sent with `"strict": false` so the model sees MCP tool schemas unchanged.

use std::fmt;
use std::time::Instant;

use futures_util::StreamExt;
use harness_core::redact::Redactor;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::common::{
    ApiKey, RetryPolicy, UNAVAILABLE_ID, build_http, cancellable, check_base_url, elapsed_ms,
    map_reqwest, post_json_with_retries, scrub,
};
use super::sse::{SseItem, SseParser};
use super::{
    AssistantTurn, ModelClient, ModelError, ModelRequest, ModelResponse, Replayed, SendFuture,
    StreamEvent, TokenUsage, ToolCall, classify_history,
};
use crate::registry::ProviderId;

/// Client for the OpenAI Responses API.
pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: ApiKey,
    redactor: Redactor,
    retry: RetryPolicy,
}

impl fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiClient {
    /// Default API base.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.openai.com/v1";

    /// Create a client. `base_url` overrides the default; it must be `https`, or `http` on a
    /// loopback host (for local test servers).
    pub fn new(api_key: String, base_url: Option<&str>) -> Result<Self, ModelError> {
        let key = ApiKey::new(api_key)?;
        let base = check_base_url("OpenAI", base_url.unwrap_or(Self::DEFAULT_BASE_URL))?;
        Ok(Self {
            http: build_http(None, std::time::Duration::from_secs(600))?,
            endpoint: format!("{base}/responses"),
            redactor: key.redactor(),
            api_key: key,
            retry: RetryPolicy::default(),
        })
    }

    /// Create a client from an API key environment variable.
    pub fn from_env(env_name: &str, base_url: Option<&str>) -> Result<Self, ModelError> {
        let key = ApiKey::from_env("openai", env_name)?;
        Self::new(key.expose().to_owned(), base_url)
    }

    /// Replace the retry policy (tests use short waits).
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Full endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl ModelClient for OpenAiClient {
    fn provider(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    fn send<'a>(
        &'a self,
        req: ModelRequest<'a>,
        stream: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> SendFuture<'a> {
        Box::pin(async move { self.run(req, stream, cancel).await })
    }
}

impl OpenAiClient {
    async fn run(
        &self,
        req: ModelRequest<'_>,
        stream: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let body = build_request_body(&req)?;
        let started = Instant::now();
        let headers = [("authorization", format!("Bearer {}", self.api_key.expose()))];
        let response = post_json_with_retries(
            &self.http,
            &self.endpoint,
            &headers,
            &body,
            &cancel,
            &self.redactor,
            &self.retry,
        )
        .await?;
        let header_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let mut parser = SseParser::new();
        let mut bytes = response.bytes_stream();
        let mut completed: Option<Value> = None;
        'read: loop {
            let chunk = match cancellable(&cancel, bytes.next()).await? {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => return Err(map_reqwest(&e, &self.redactor)),
                None => break,
            };
            for item in parser.push(&chunk) {
                if self.handle_item(item, &stream, &mut completed)? {
                    break 'read;
                }
            }
        }
        if completed.is_none() {
            for item in parser.finish() {
                self.handle_item(item, &stream, &mut completed)?;
            }
        }
        let response_json = completed.ok_or_else(|| {
            ModelError::InvalidResponse("stream ended before response.completed".to_owned())
        })?;
        let mut parsed = parse_completed(&response_json, &req.spec.alias)?;
        if parsed.request_id == UNAVAILABLE_ID
            && let Some(id) = header_id
        {
            parsed.request_id = id;
        }
        Ok(ModelResponse {
            turn: parsed.turn,
            usage: parsed.usage,
            request_id: parsed.request_id,
            latency_ms: elapsed_ms(started),
        })
    }

    /// Handle one SSE item. Returns `true` when the stream should stop.
    fn handle_item(
        &self,
        item: SseItem,
        stream: &UnboundedSender<StreamEvent>,
        completed: &mut Option<Value>,
    ) -> Result<bool, ModelError> {
        let event = match item {
            SseItem::Done => return Ok(true),
            SseItem::Event(event) => event,
        };
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|e| ModelError::InvalidResponse(format!("event data is not JSON: {e}")))?;
        let kind = data
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    let _ = stream.send(StreamEvent::TextDelta(delta.to_owned()));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = data.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let _ = stream.send(StreamEvent::ToolCallStarted {
                        call_id: str_field(item, "call_id"),
                        name: str_field(item, "name"),
                    });
                }
            }
            "response.completed" => {
                let response = data.get("response").cloned().ok_or_else(|| {
                    ModelError::InvalidResponse("response.completed has no response".to_owned())
                })?;
                *completed = Some(response);
                return Ok(true);
            }
            "response.failed" | "response.incomplete" => {
                let response = data.get("response");
                let message = response
                    .and_then(|r| r.pointer("/error/message"))
                    .or_else(|| response.and_then(|r| r.pointer("/incomplete_details/reason")))
                    .and_then(Value::as_str)
                    .unwrap_or("no details");
                return Err(ModelError::InvalidResponse(format!(
                    "{kind}: {}",
                    scrub(&self.redactor, message)
                )));
            }
            "error" => {
                let message = data
                    .get("message")
                    .or_else(|| data.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("no details");
                return Err(ModelError::InvalidResponse(format!(
                    "error event: {}",
                    scrub(&self.redactor, message)
                )));
            }
            _ => {}
        }
        Ok(false)
    }
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Build the JSON body of a Responses request.
pub(crate) fn build_request_body(req: &ModelRequest<'_>) -> Result<Value, ModelError> {
    let spec = req.spec;
    if !spec.effort_supported(req.effort) {
        return Err(ModelError::UnsupportedEffort {
            model: spec.alias.clone(),
            effort: req.effort,
        });
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(spec.model_id));
    body.insert("instructions".to_owned(), json!(req.instructions));
    body.insert("input".to_owned(), Value::Array(build_input(req)));
    body.insert("stream".to_owned(), json!(true));
    body.insert("store".to_owned(), json!(false));
    body.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
    body.insert(
        "reasoning".to_owned(),
        json!({"effort": req.effort.as_str()}),
    );
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    // temperature, top_p and seed are never sent: `ModelRequest` carries no sampling fields and
    // gpt-6-astra rejects them.
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(schema) = req.json_schema {
        body.insert(
            "text".to_owned(),
            json!({"format": {
                "type": "json_schema",
                "name": "structured_output",
                "strict": false,
                "schema": schema,
            }}),
        );
    }
    Ok(Value::Object(body))
}

fn build_input(req: &ModelRequest<'_>) -> Vec<Value> {
    let mut input = Vec::new();
    for replayed in classify_history(req.history, &req.spec.alias) {
        match replayed {
            Replayed::User(text) => input.push(json!({"role": "user", "content": text})),
            Replayed::Own(turn) => input.extend(turn.raw.iter().cloned()),
            Replayed::Foreign(turn) => {
                if !turn.text.is_empty() {
                    input.push(json!({"role": "assistant", "content": turn.text}));
                }
                for call in &turn.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.call_id,
                        "name": call.name,
                        "arguments": call.raw_arguments,
                    }));
                }
            }
            Replayed::ToolResult { result, .. } => input.push(json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": result.output,
            })),
        }
    }
    input
}

struct Parsed {
    turn: AssistantTurn,
    usage: TokenUsage,
    request_id: String,
}

fn count(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

/// Turn the `response` object of `response.completed` into a neutral turn.
fn parse_completed(response: &Value, alias: &str) -> Result<Parsed, ModelError> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| ModelError::InvalidResponse("response has no output array".to_owned()))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(t) = part.get("text").and_then(Value::as_str)
                        {
                            text.push_str(t);
                        }
                    }
                }
            }
            Some("function_call") => {
                let raw_arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = if raw_arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&raw_arguments)
                        .unwrap_or_else(|_| Value::String(raw_arguments.clone()))
                };
                tool_calls.push(ToolCall {
                    call_id: str_field(item, "call_id"),
                    name: str_field(item, "name"),
                    arguments,
                    raw_arguments,
                });
            }
            _ => {}
        }
    }
    let usage = response.get("usage");
    let token_usage = usage.map_or_else(TokenUsage::default, |u| TokenUsage {
        input: count(u.get("input_tokens")),
        cached_input: count(u.pointer("/input_tokens_details/cached_tokens")),
        cache_write: count(u.pointer("/input_tokens_details/cache_write_tokens")),
        output_including_reasoning: count(u.get("output_tokens")),
        reasoning: count(u.pointer("/output_tokens_details/reasoning_tokens")),
    });
    let request_id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(UNAVAILABLE_ID)
        .to_owned();
    Ok(Parsed {
        turn: AssistantTurn {
            model_alias: alias.to_owned(),
            text,
            tool_calls,
            raw: output.clone(),
        },
        usage: token_usage,
        request_id,
    })
}
