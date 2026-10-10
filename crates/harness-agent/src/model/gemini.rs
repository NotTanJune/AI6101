//! Gemini Interactions API client (`POST {base}/interactions`, non-streaming).
//!
//! Streaming is deliberately off: the streamed `interaction.completed` event does not carry
//! `steps`, and rebuilding steps from deltas could break the "exactly as received" replay rule
//! that thought signatures depend on. Requests are stateless (`store: false`), so every request
//! resends the user input, the model's steps and the `function_result` steps.

use std::fmt;
use std::time::{Duration, Instant};

use harness_core::redact::Redactor;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::common::{
    ApiKey, RetryPolicy, UNAVAILABLE_ID, build_http, cancellable, check_base_url, elapsed_ms,
    map_reqwest, post_json_with_retries, scrub,
};
use super::{
    AssistantTurn, ModelClient, ModelError, ModelRequest, ModelResponse, Replayed, SendFuture,
    StreamEvent, TokenUsage, ToolCall, classify_history,
};
use crate::registry::{Effort, ProviderId};

/// Client for the Gemini Interactions API.
pub struct GeminiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: ApiKey,
    redactor: Redactor,
    retry: RetryPolicy,
}

impl fmt::Debug for GeminiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl GeminiClient {
    /// Default API base.
    pub const DEFAULT_BASE_URL: &'static str = "https://generativelanguage.googleapis.com/v1beta";

    /// Create a client. `base_url` overrides the default; it must be `https`, or `http` on a
    /// loopback host (for local test servers).
    pub fn new(api_key: String, base_url: Option<&str>) -> Result<Self, ModelError> {
        let key = ApiKey::new(api_key)?;
        let base = check_base_url("Gemini", base_url.unwrap_or(Self::DEFAULT_BASE_URL))?;
        Ok(Self {
            http: build_http(Some(Duration::from_secs(900)), Duration::from_secs(900))?,
            endpoint: format!("{base}/interactions"),
            redactor: key.redactor(),
            api_key: key,
            retry: RetryPolicy::default(),
        })
    }

    /// Create a client from an API key environment variable.
    pub fn from_env(env_name: &str, base_url: Option<&str>) -> Result<Self, ModelError> {
        let key = ApiKey::from_env("gemini", env_name)?;
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

impl ModelClient for GeminiClient {
    fn provider(&self) -> ProviderId {
        ProviderId::Gemini
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

impl GeminiClient {
    async fn run(
        &self,
        req: ModelRequest<'_>,
        stream: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        // Built before any HTTP call so an unsupported effort fails without a request.
        let body = build_request_body(&req)?;
        let started = Instant::now();
        let headers = [("x-goog-api-key", self.api_key.expose().to_owned())];
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
        let text = cancellable(&cancel, response.text())
            .await?
            .map_err(|e| map_reqwest(&e, &self.redactor))?;
        let json: Value = serde_json::from_str(&text).map_err(|e| {
            ModelError::InvalidResponse(format!(
                "body is not JSON: {}",
                scrub(&self.redactor, &e.to_string())
            ))
        })?;
        let mut parsed = parse_interaction(&json, &req.spec.alias, &self.redactor)?;
        if parsed.request_id == UNAVAILABLE_ID
            && let Some(id) = header_id
        {
            parsed.request_id = id;
        }
        if !parsed.turn.text.is_empty() {
            let _ = stream.send(StreamEvent::TextDelta(parsed.turn.text.clone()));
        }
        Ok(ModelResponse {
            turn: parsed.turn,
            usage: parsed.usage,
            request_id: parsed.request_id,
            latency_ms: elapsed_ms(started),
        })
    }
}

fn text_content(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

/// Build the JSON body of an Interactions request.
pub(crate) fn build_request_body(req: &ModelRequest<'_>) -> Result<Value, ModelError> {
    let spec = req.spec;
    if !matches!(req.effort, Effort::Low | Effort::Medium | Effort::High)
        || !spec.effort_supported(req.effort)
    {
        return Err(ModelError::UnsupportedEffort {
            model: spec.alias.clone(),
            effort: req.effort,
        });
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(spec.model_id));
    // The Interactions API has a top-level `system_instruction` string field.
    body.insert("system_instruction".to_owned(), json!(req.instructions));
    body.insert("input".to_owned(), Value::Array(build_input(req)));
    body.insert("store".to_owned(), json!(false));
    body.insert("stream".to_owned(), json!(false));
    body.insert(
        "generation_config".to_owned(),
        json!({
            "thinking_level": req.effort.as_str(),
            "max_output_tokens": req.max_output_tokens,
        }),
    );
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
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(schema) = req.json_schema {
        body.insert(
            "response_format".to_owned(),
            json!({"type": "text", "mime_type": "application/json", "schema": schema}),
        );
    }
    Ok(Value::Object(body))
}

fn build_input(req: &ModelRequest<'_>) -> Vec<Value> {
    let mut input = Vec::new();
    // Foreign turns (and the tool results that answer their calls) become one plain
    // transcript step: Gemini may reject function-call steps that lack thought signatures.
    let mut transcript: Vec<String> = Vec::new();
    let flush = |transcript: &mut Vec<String>, input: &mut Vec<Value>| {
        if !transcript.is_empty() {
            input.push(json!({
                "type": "user_input",
                "content": text_content(&transcript.join("\n")),
            }));
            transcript.clear();
        }
    };
    for replayed in classify_history(req.history, &req.spec.alias) {
        match replayed {
            Replayed::Foreign(turn) => {
                if !turn.text.is_empty() {
                    transcript.push(format!("Assistant said: {}", turn.text));
                }
                for call in &turn.tool_calls {
                    transcript.push(format!(
                        "Assistant called {} with {}",
                        call.name, call.raw_arguments
                    ));
                }
            }
            Replayed::ToolResult {
                result,
                foreign: true,
            } => transcript.push(format!("Tool {} returned: {}", result.name, result.output)),
            other => {
                flush(&mut transcript, &mut input);
                match other {
                    Replayed::User(text) => input.push(json!({
                        "type": "user_input",
                        "content": text_content(text),
                    })),
                    Replayed::Own(turn) => input.extend(turn.raw.iter().cloned()),
                    Replayed::ToolResult { result, .. } => input.push(json!({
                        "type": "function_result",
                        "name": result.name,
                        "call_id": result.call_id,
                        "result": text_content(result.output),
                    })),
                    Replayed::Foreign(_) => {}
                }
            }
        }
    }
    flush(&mut transcript, &mut input);
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

fn parse_interaction(
    interaction: &Value,
    alias: &str,
    redactor: &Redactor,
) -> Result<Parsed, ModelError> {
    let status = interaction
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    if status != "completed" && status != "requires_action" {
        let detail = interaction
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("no details");
        return Err(ModelError::InvalidResponse(format!(
            "interaction status {status}: {}",
            scrub(redactor, detail)
        )));
    }
    let steps = interaction
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| ModelError::InvalidResponse("interaction has no steps array".to_owned()))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for step in steps {
        match step.get("type").and_then(Value::as_str) {
            Some("model_output") => {
                if let Some(parts) = step.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(t) = part.get("text").and_then(Value::as_str)
                        {
                            text.push_str(t);
                        }
                    }
                }
            }
            Some("function_call") => {
                let arguments = step.get("arguments").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(ToolCall {
                    call_id: step
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: step
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    raw_arguments: arguments.to_string(),
                    arguments,
                });
            }
            _ => {}
        }
    }
    let usage = interaction.get("usage");
    let token_usage = usage.map_or_else(TokenUsage::default, |u| {
        let output = count(u.get("total_output_tokens"));
        let thought = count(u.get("total_thought_tokens"));
        // Thought tokens bill at the output rate and are not part of `total_output_tokens`.
        let output_including_reasoning = match (output, thought) {
            (None, None) => None,
            (o, t) => Some(o.unwrap_or(0) + t.unwrap_or(0)),
        };
        TokenUsage {
            input: count(u.get("total_input_tokens")),
            cached_input: count(u.get("total_cached_tokens")),
            cache_write: None,
            output_including_reasoning,
            reasoning: thought,
        }
    });
    let request_id = interaction
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
            raw: steps.clone(),
        },
        usage: token_usage,
        request_id,
    })
}
