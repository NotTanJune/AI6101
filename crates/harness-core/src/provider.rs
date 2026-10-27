//! Model providers: the [`Provider`] trait, a deterministic [`MockProvider`], HTTP providers
//! for OpenAI, Gemini and local OpenAI-compatible servers, and [`MeteredProvider`], the only
//! way adaptation code talks to a provider.
//!
//! Rules enforced here:
//! * API keys are read once (by the caller, from an environment variable whose NAME is in the
//!   config), held in a type whose `Debug` output is redacted, sent only in request headers and
//!   scrubbed from every error text.
//! * Unknown usage is never turned into zero: a paid call without usage is settled as
//!   `unknown` and keeps counting against the local dispatch cap.
//! * A refused reservation makes zero provider calls.
//! * HTTP request bodies and response parsing are pure functions, unit-tested on canned JSON.
//!   Tests never touch the network.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{ProviderKind as ConfigProviderKind, ProviderSection, Strategy};
use crate::cost::{CostError, CostLedger, ProviderKind, ReserveRequest, Usage};
use crate::events::{EventBody, EventEmitter, EventError, Phase};
use crate::redact::Redactor;

/// Output-token limit used when the config does not set one (mock and local providers).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// Request timeout for HTTP providers.
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest provider error text kept in a [`ProviderError`].
const MAX_ERROR_TEXT: usize = 300;

// ---------------------------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------------------------

/// One completion request.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionRequest {
    /// What the call is for (`full_rewrite`, `selective_patch`, ...). The mock provider selects
    /// its canned response by this tag; real providers ignore it.
    pub purpose: String,
    /// System prompt.
    pub system: String,
    /// User prompt.
    pub user: String,
    /// Output token limit (also used to reserve budget).
    pub max_output_tokens: u32,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling value.
    pub top_p: Option<f64>,
    /// Provider-side sampling seed.
    pub seed: Option<u64>,
    /// JSON Schema for structured output, when the caller needs it.
    pub json_schema: Option<Value>,
}

impl CompletionRequest {
    /// Conservative input-token estimate: characters of prompt and schema divided by four,
    /// rounded up.
    pub fn estimated_input_tokens(&self) -> u64 {
        let schema_chars = self
            .json_schema
            .as_ref()
            .map_or(0, |s| s.to_string().chars().count());
        let chars = self.system.chars().count() + self.user.chars().count() + schema_chars;
        (chars as u64).div_ceil(4)
    }
}

/// Token usage reported by a provider. Each figure is `None` when the provider did not
/// return it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageReport {
    /// Input (prompt) tokens.
    pub input_tokens: Option<u64>,
    /// Output tokens, including hidden reasoning tokens when the provider bills them.
    pub output_tokens: Option<u64>,
}

/// A completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    /// Generated text.
    pub text: String,
    /// Usage, when the provider returned any.
    pub usage: Option<UsageReport>,
    /// Provider request id (a placeholder when the provider returned none).
    pub request_id: String,
    /// Wall-clock latency.
    pub latency_ms: u64,
}

/// Provider failure. Messages never contain the API key.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The environment variable that should hold the API key is unset or empty.
    #[error("environment variable {env_name} is not set; cannot authenticate to {provider}")]
    MissingApiKey {
        /// Provider name.
        provider: String,
        /// Variable name (never its value).
        env_name: String,
    },
    /// Provider configuration problem.
    #[error("provider configuration: {0}")]
    Config(String),
    /// The server answered with a non-success status.
    #[error("HTTP {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Server message, redacted and truncated.
        message: String,
    },
    /// The request timed out.
    #[error("request timed out")]
    Timeout,
    /// The request could not be completed (connection, TLS, I/O).
    #[error("transport error: {0}")]
    Transport(String),
    /// The server answered successfully but the body is not a usable completion.
    #[error("unusable provider response: {0}")]
    InvalidResponse(String),
    /// Mock provider has nothing to return.
    #[error("mock provider: {0}")]
    Mock(String),
}

impl ProviderError {
    /// Whether the provider may have processed (and billed) the request even though no usage
    /// came back: timeouts, server errors and unusable success responses.
    pub fn billing_uncertain(&self) -> bool {
        match self {
            Self::Timeout | Self::InvalidResponse(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// A model provider. Adaptation code never calls this directly; it goes through
/// [`MeteredProvider`].
pub trait Provider: Send {
    /// Cost kind used for budget reservations.
    fn kind(&self) -> ProviderKind;
    /// Provider name (matches rate card entries and event `provider` fields).
    fn id(&self) -> &str;
    /// Model id.
    fn model(&self) -> &str;
    /// Run one completion.
    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;
}

/// Decoding settings held fixed for a comparison run.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoding {
    /// Output token limit.
    pub max_output_tokens: u32,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling value.
    pub top_p: Option<f64>,
    /// Sampling seed.
    pub seed: Option<u64>,
}

impl Default for Decoding {
    fn default() -> Self {
        Self {
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            temperature: Some(0.0),
            top_p: None,
            seed: None,
        }
    }
}

impl Decoding {
    /// Decoding settings from the `[provider]` config section.
    pub fn from_section(section: &ProviderSection) -> Self {
        Self {
            max_output_tokens: section
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            temperature: section.temperature,
            top_p: section.top_p,
            seed: section.seed,
        }
    }

    /// Build a request with these settings.
    pub fn request(
        &self,
        purpose: &str,
        system: String,
        user: String,
        json_schema: Option<Value>,
    ) -> CompletionRequest {
        CompletionRequest {
            purpose: purpose.to_owned(),
            system,
            user,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
            json_schema,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// MeteredProvider
// ---------------------------------------------------------------------------------------------

/// Failure of a metered call.
#[derive(Debug, thiserror::Error)]
pub enum MeteredError {
    /// The local dispatch cap or a missing rate refused the call. The provider was NOT called.
    #[error("dispatch refused: {0}")]
    Refused(#[source] CostError),
    /// The provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The `model_call` event could not be written.
    #[error(transparent)]
    Event(#[from] EventError),
}

impl MeteredError {
    /// Whether the call was refused by budget rules before reaching the provider.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused(_))
    }
}

/// Wraps a provider with budget reservation, settlement and `model_call` events.
///
/// Per call: reserve (input tokens estimated from prompt length, output limit from the
/// request) -> complete -> settle or release -> emit one `model_call` event.
pub struct MeteredProvider<'a> {
    provider: &'a mut dyn Provider,
    ledger: &'a mut CostLedger,
    emitter: &'a mut EventEmitter,
    strategy: Option<Strategy>,
    decoding: Decoding,
}

impl<'a> MeteredProvider<'a> {
    /// Wrap `provider`. `strategy` tags the emitted events.
    pub fn new(
        provider: &'a mut dyn Provider,
        ledger: &'a mut CostLedger,
        emitter: &'a mut EventEmitter,
        strategy: Option<Strategy>,
    ) -> Self {
        Self {
            provider,
            ledger,
            emitter,
            strategy,
            decoding: Decoding::default(),
        }
    }

    /// Set the fixed decoding settings used by strategies.
    #[must_use]
    pub fn with_decoding(mut self, decoding: Decoding) -> Self {
        self.decoding = decoding;
        self
    }

    /// Decoding settings strategies must use for their requests.
    pub fn decoding(&self) -> &Decoding {
        &self.decoding
    }

    /// Provider name.
    pub fn provider_id(&self) -> &str {
        self.provider.id()
    }

    /// Model id.
    pub fn model(&self) -> &str {
        self.provider.model()
    }

    /// The ledger this provider settles against.
    pub fn ledger(&self) -> &CostLedger {
        self.ledger
    }

    /// Run one metered call.
    pub fn call(
        &mut self,
        phase: Phase,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, MeteredError> {
        let kind = self.provider.kind();
        let reservation = self
            .ledger
            .reserve(ReserveRequest {
                provider: self.provider.id().to_owned(),
                model: self.provider.model().to_owned(),
                phase,
                input_tokens_estimate: req.estimated_input_tokens(),
                max_output_tokens: req.max_output_tokens,
                kind,
            })
            .map_err(MeteredError::Refused)?;

        match self.provider.complete(req) {
            Ok(response) => {
                let usage = Usage {
                    request_id: non_empty_id(&response.request_id),
                    input_tokens: response.usage.and_then(|u| u.input_tokens),
                    output_tokens: response.usage.and_then(|u| u.output_tokens),
                    cached_input_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                    latency_ms: response.latency_ms,
                };
                let record = self.ledger.settle(reservation, usage);
                self.emitter
                    .emit(self.strategy, EventBody::ModelCall(record))?;
                Ok(response)
            }
            Err(error) => {
                if kind == ProviderKind::Paid && error.billing_uncertain() {
                    // The provider may have billed this call: keep the reservation counting
                    // against the cap and record the call as unknown, never as free.
                    let record = self.ledger.settle(
                        reservation,
                        Usage {
                            request_id: UNAVAILABLE_ID.to_owned(),
                            input_tokens: None,
                            output_tokens: None,
                            cached_input_tokens: None,
                            cache_write_tokens: None,
                            reasoning_tokens: None,
                            latency_ms: 0,
                        },
                    );
                    self.emitter
                        .emit(self.strategy, EventBody::ModelCall(record))?;
                } else {
                    self.ledger.release(reservation);
                }
                Err(MeteredError::Provider(error))
            }
        }
    }
}

const UNAVAILABLE_ID: &str = "unavailable";

fn non_empty_id(id: &str) -> String {
    if id.trim().is_empty() {
        UNAVAILABLE_ID.to_owned()
    } else {
        id.to_owned()
    }
}

// ---------------------------------------------------------------------------------------------
// MockProvider
// ---------------------------------------------------------------------------------------------

/// Deterministic in-process provider for tests and the offline smoke run. Makes no network
/// request. Token counts are derived from text length so cost events are populated.
pub struct MockProvider {
    id: String,
    model: String,
    kind: ProviderKind,
    report_usage: bool,
    script: VecDeque<Result<String, ProviderError>>,
    by_purpose: BTreeMap<String, String>,
    calls: usize,
    requests: Vec<CompletionRequest>,
}

impl fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockProvider")
            .field("id", &self.id)
            .field("model", &self.model)
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

impl MockProvider {
    fn empty() -> Self {
        Self {
            id: "mock".to_owned(),
            model: "mock-1".to_owned(),
            kind: ProviderKind::Mock,
            report_usage: true,
            script: VecDeque::new(),
            by_purpose: BTreeMap::new(),
            calls: 0,
            requests: Vec::new(),
        }
    }

    /// Return the given texts in order, one per call; a further call is an error.
    pub fn scripted(responses: Vec<String>) -> Self {
        Self::scripted_results(responses.into_iter().map(Ok).collect())
    }

    /// Like [`MockProvider::scripted`] but entries may be failures.
    pub fn scripted_results(responses: Vec<Result<String, ProviderError>>) -> Self {
        Self {
            script: responses.into(),
            ..Self::empty()
        }
    }

    /// Load `<purpose>.json` files from `dir` (for example `full_rewrite.json` and
    /// `selective_patch.json`); a request is answered with the file named by its `purpose`.
    pub fn from_fixture_dir(dir: impl AsRef<Path>) -> Result<Self, ProviderError> {
        let dir = dir.as_ref();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| ProviderError::Mock(format!("cannot read {}: {e}", dir.display())))?;
        let mut mock = Self::empty();
        for entry in entries {
            let path = entry
                .map_err(|e| ProviderError::Mock(format!("cannot list {}: {e}", dir.display())))?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ProviderError::Mock(format!("cannot read {}: {e}", path.display())))?;
            mock.by_purpose.insert(stem.to_owned(), text);
        }
        if mock.by_purpose.is_empty() {
            return Err(ProviderError::Mock(format!(
                "no <purpose>.json files in {}",
                dir.display()
            )));
        }
        Ok(mock)
    }

    /// Pretend to be another provider/model (for ledger tests).
    #[must_use]
    pub fn with_identity(mut self, id: impl Into<String>, model: impl Into<String>) -> Self {
        self.id = id.into();
        self.model = model.into();
        self
    }

    /// Report another cost kind (for example `Paid` to exercise pricing).
    #[must_use]
    pub fn with_kind(mut self, kind: ProviderKind) -> Self {
        self.kind = kind;
        self
    }

    /// Return responses without usage, like a provider that omits token counts.
    #[must_use]
    pub fn without_usage(mut self) -> Self {
        self.report_usage = false;
        self
    }

    /// Number of `complete` calls received (including failed ones).
    pub fn call_count(&self) -> usize {
        self.calls
    }

    /// Requests received so far.
    pub fn requests(&self) -> &[CompletionRequest] {
        &self.requests
    }
}

impl Provider for MockProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls += 1;
        self.requests.push(req.clone());
        let text = match self.script.pop_front() {
            Some(entry) => entry?,
            None => self.by_purpose.get(&req.purpose).cloned().ok_or_else(|| {
                ProviderError::Mock(format!("no response for purpose {:?}", req.purpose))
            })?,
        };
        let usage = self.report_usage.then(|| UsageReport {
            input_tokens: Some(req.estimated_input_tokens()),
            output_tokens: Some((text.chars().count() as u64).div_ceil(4)),
        });
        Ok(CompletionResponse {
            text,
            usage,
            request_id: format!("mock-{}", self.calls),
            latency_ms: 0,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Pure request builders and response parsers
// ---------------------------------------------------------------------------------------------

/// Parsed provider response body.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedResponse {
    /// Generated text.
    pub text: String,
    /// Usage when reported.
    pub usage: Option<UsageReport>,
    /// Provider-assigned response id, when present.
    pub request_id: Option<String>,
}

/// Name of the output-limit parameter in an OpenAI-style request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLimitParam {
    /// `max_completion_tokens` (OpenAI).
    MaxCompletionTokens,
    /// `max_tokens` (most OpenAI-compatible local servers).
    MaxTokens,
}

impl TokenLimitParam {
    fn key(self) -> &'static str {
        match self {
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxTokens => "max_tokens",
        }
    }
}

/// Request body for `POST {base}/chat/completions`.
pub fn build_openai_request(
    model: &str,
    req: &CompletionRequest,
    limit_param: TokenLimitParam,
) -> Value {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }
    messages.push(json!({ "role": "user", "content": req.user }));
    let mut body = json!({ "model": model, "messages": messages });
    body[limit_param.key()] = json!(req.max_output_tokens);
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = json!(p);
    }
    if let Some(s) = req.seed {
        body["seed"] = json!(s);
    }
    if let Some(schema) = &req.json_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": "structured_output", "schema": schema, "strict": false }
        });
    }
    body
}

fn count(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

/// Parse an OpenAI-style chat completion body.
pub fn parse_openai_response(body: &Value) -> Result<ParsedResponse, ProviderError> {
    let message = body
        .pointer("/choices/0/message")
        .ok_or_else(|| ProviderError::InvalidResponse("no choices[0].message".to_owned()))?;
    let text = match message.get("content").and_then(Value::as_str) {
        Some(t) => t.to_owned(),
        None => {
            let reason = if message.get("refusal").is_some_and(|r| !r.is_null()) {
                "the model refused the request"
            } else {
                "message has no text content"
            };
            return Err(ProviderError::InvalidResponse(reason.to_owned()));
        }
    };
    let usage = body.get("usage").filter(|u| u.is_object()).and_then(|u| {
        let report = UsageReport {
            input_tokens: count(u.get("prompt_tokens")),
            output_tokens: count(u.get("completion_tokens")),
        };
        (report.input_tokens.is_some() || report.output_tokens.is_some()).then_some(report)
    });
    let request_id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(ParsedResponse {
        text,
        usage,
        request_id,
    })
}

/// Request body for Gemini `models/{model}:generateContent`.
pub fn build_gemini_request(req: &CompletionRequest) -> Value {
    let mut generation = json!({ "maxOutputTokens": req.max_output_tokens });
    if let Some(t) = req.temperature {
        generation["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        generation["topP"] = json!(p);
    }
    if let Some(s) = req.seed {
        generation["seed"] = json!(s);
    }
    if let Some(schema) = &req.json_schema {
        generation["responseMimeType"] = json!("application/json");
        generation["responseJsonSchema"] = schema.clone();
    }
    let mut body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": req.user }] }],
        "generationConfig": generation,
    });
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": req.system }] });
    }
    body
}

/// Parse a Gemini `generateContent` body. Hidden reasoning tokens are billed as output, so
/// `output_tokens` is candidates plus thoughts; it is `None` when candidate tokens are absent.
pub fn parse_gemini_response(body: &Value) -> Result<ParsedResponse, ProviderError> {
    let Some(candidate) = body.pointer("/candidates/0") else {
        let reason = body
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .map_or_else(
                || "no candidates".to_owned(),
                |r| format!("prompt blocked: {r}"),
            );
        return Err(ProviderError::InvalidResponse(reason));
    };
    let text: String = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        let finish = candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ProviderError::InvalidResponse(format!(
            "candidate has no text (finishReason {finish})"
        )));
    }
    let usage = body
        .get("usageMetadata")
        .filter(|u| u.is_object())
        .and_then(|u| {
            let output = count(u.get("candidatesTokenCount"))
                .map(|c| c + count(u.get("thoughtsTokenCount")).unwrap_or(0));
            let report = UsageReport {
                input_tokens: count(u.get("promptTokenCount")),
                output_tokens: output,
            };
            (report.input_tokens.is_some() || report.output_tokens.is_some()).then_some(report)
        });
    let request_id = body
        .get("responseId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(ParsedResponse {
        text,
        usage,
        request_id,
    })
}

// ---------------------------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------------------------

/// API key. `Debug` never shows the value; there is no `Display`.
#[derive(Clone)]
struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

/// Raw HTTP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    /// Status code.
    pub status: u16,
    /// Response body text.
    pub body: String,
    /// Value of an `x-request-id` header, when present.
    pub request_id_header: Option<String>,
}

/// Blocking JSON `POST`. Implemented by [`UreqTransport`]; tests substitute a fake so no test
/// touches the network. Header values may contain secrets: implementations must not log them.
pub trait HttpTransport: Send {
    /// POST `body` as JSON to `url` with the given headers.
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpReply, ProviderError>;
}

/// `ureq` (rustls) transport.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqTransport")
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl HttpTransport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpReply, ProviderError> {
        let map_err = |e: ureq::Error| match e {
            ureq::Error::Timeout(_) => ProviderError::Timeout,
            other => ProviderError::Transport(other.to_string()),
        };
        let mut request = self.agent.post(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let mut response = request.send_json(body).map_err(map_err)?;
        let status = response.status().as_u16();
        let request_id_header = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.body_mut().read_to_string().map_err(map_err)?;
        Ok(HttpReply {
            status,
            body,
            request_id_header,
        })
    }
}

/// Shared HTTP call logic: send, map failures, scrub secrets.
struct HttpClient {
    transport: Box<dyn HttpTransport>,
    key: Option<ApiKey>,
    redactor: Redactor,
}

impl HttpClient {
    fn new(key: Option<String>) -> Self {
        let redactor = Redactor::new(key.iter().cloned().collect());
        Self {
            transport: Box::new(UreqTransport::default()),
            key: key.map(ApiKey),
            redactor,
        }
    }

    fn scrub(&self, text: &str) -> String {
        let cleaned = self.redactor.redact_str(text);
        cleaned.chars().take(MAX_ERROR_TEXT).collect()
    }

    /// POST and return the parsed JSON body plus the request-id header.
    fn post(
        &self,
        url: &str,
        mut headers: Vec<(String, String)>,
        body: &Value,
    ) -> Result<(Value, Option<String>), ProviderError> {
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
        let reply = self
            .transport
            .post_json(url, &headers, body)
            .map_err(|e| match e {
                ProviderError::Transport(m) => ProviderError::Transport(self.scrub(&m)),
                other => other,
            })?;
        if !(200..300).contains(&reply.status) {
            let text = serde_json::from_str::<Value>(&reply.body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or(reply.body);
            return Err(ProviderError::Http {
                status: reply.status,
                message: self.scrub(&text),
            });
        }
        let parsed = serde_json::from_str::<Value>(&reply.body)
            .map_err(|e| ProviderError::InvalidResponse(format!("body is not JSON: {e}")))?;
        Ok((parsed, reply.request_id_header))
    }

    fn bearer(&self) -> Vec<(String, String)> {
        self.key
            .iter()
            .map(|k| ("authorization".to_owned(), format!("Bearer {}", k.0)))
            .collect()
    }
}

fn finish(
    parsed: ParsedResponse,
    header_id: Option<String>,
    started: Instant,
) -> CompletionResponse {
    CompletionResponse {
        text: parsed.text,
        usage: parsed.usage,
        request_id: parsed
            .request_id
            .or(header_id)
            .unwrap_or_else(|| UNAVAILABLE_ID.to_owned()),
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

fn require_https(what: &str, url: &str) -> Result<(), ProviderError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(ProviderError::Config(format!(
            "{what} base URL must use https because an API key is sent to it"
        )))
    }
}

fn chat_endpoint(base: &str) -> String {
    format!("{}/chat/completions", base.trim_end_matches('/'))
}

/// OpenAI chat-completions provider (paid).
pub struct OpenAiProvider {
    model: String,
    endpoint: String,
    http: HttpClient,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Default API base.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.openai.com/v1";

    /// Create a provider. `base_url` overrides the default and must be `https`.
    pub fn new(
        model: impl Into<String>,
        api_key: String,
        base_url: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let base = base_url.unwrap_or(Self::DEFAULT_BASE_URL);
        require_https("OpenAI", base)?;
        if api_key.trim().is_empty() {
            return Err(ProviderError::Config("API key is empty".to_owned()));
        }
        Ok(Self {
            model: model.into(),
            endpoint: chat_endpoint(base),
            http: HttpClient::new(Some(api_key)),
        })
    }

    /// Replace the transport (tests).
    #[must_use]
    pub fn with_transport(mut self, transport: Box<dyn HttpTransport>) -> Self {
        self.http.transport = transport;
        self
    }

    /// Full request URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Paid
    }

    fn id(&self) -> &str {
        ConfigProviderKind::OpenAi.name()
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let started = Instant::now();
        let body = build_openai_request(&self.model, req, TokenLimitParam::MaxCompletionTokens);
        let (reply, header_id) = self.http.post(&self.endpoint, self.http.bearer(), &body)?;
        Ok(finish(parse_openai_response(&reply)?, header_id, started))
    }
}

/// Local OpenAI-compatible endpoint (llama.cpp, vLLM, Ollama and similar). Inference time is
/// tracked, never USD.
pub struct LocalProvider {
    model: String,
    endpoint: String,
    http: HttpClient,
}

impl fmt::Debug for LocalProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalProvider")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl LocalProvider {
    /// Create a provider for `base_url` (for example `http://localhost:8080/v1`). The key is
    /// optional; when present it is sent as a bearer token, so `https` is then required unless
    /// the host is loopback.
    pub fn new(
        model: impl Into<String>,
        base_url: &str,
        api_key: Option<String>,
    ) -> Result<Self, ProviderError> {
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(ProviderError::Config(
                "local base URL must start with http:// or https://".to_owned(),
            ));
        }
        if api_key.is_some() && !base_url.starts_with("https://") && !is_loopback(base_url) {
            return Err(ProviderError::Config(
                "local base URL must use https when an API key is sent to a non-loopback host"
                    .to_owned(),
            ));
        }
        Ok(Self {
            model: model.into(),
            endpoint: chat_endpoint(base_url),
            http: HttpClient::new(api_key.filter(|k| !k.trim().is_empty())),
        })
    }

    /// Replace the transport (tests).
    #[must_use]
    pub fn with_transport(mut self, transport: Box<dyn HttpTransport>) -> Self {
        self.http.transport = transport;
        self
    }

    /// Full request URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

fn is_loopback(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or("");
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

impl Provider for LocalProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }

    fn id(&self) -> &str {
        ConfigProviderKind::Local.name()
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let started = Instant::now();
        let body = build_openai_request(&self.model, req, TokenLimitParam::MaxTokens);
        let (reply, header_id) = self.http.post(&self.endpoint, self.http.bearer(), &body)?;
        Ok(finish(parse_openai_response(&reply)?, header_id, started))
    }
}

/// Google Gemini `generateContent` provider (paid). The key travels in the `x-goog-api-key`
/// header, never in the URL.
pub struct GeminiProvider {
    model: String,
    endpoint: String,
    http: HttpClient,
}

impl fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl GeminiProvider {
    /// Default API base.
    pub const DEFAULT_BASE_URL: &'static str = "https://generativelanguage.googleapis.com/v1beta";

    /// Create a provider. `base_url` overrides the default and must be `https`.
    pub fn new(
        model: impl Into<String>,
        api_key: String,
        base_url: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let base = base_url.unwrap_or(Self::DEFAULT_BASE_URL);
        require_https("Gemini", base)?;
        if api_key.trim().is_empty() {
            return Err(ProviderError::Config("API key is empty".to_owned()));
        }
        let model: String = model.into();
        let short = model.strip_prefix("models/").unwrap_or(&model).to_owned();
        Ok(Self {
            endpoint: format!(
                "{}/models/{short}:generateContent",
                base.trim_end_matches('/')
            ),
            model: short,
            http: HttpClient::new(Some(api_key)),
        })
    }

    /// Replace the transport (tests).
    #[must_use]
    pub fn with_transport(mut self, transport: Box<dyn HttpTransport>) -> Self {
        self.http.transport = transport;
        self
    }

    /// Full request URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Provider for GeminiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Paid
    }

    fn id(&self) -> &str {
        ConfigProviderKind::Gemini.name()
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let started = Instant::now();
        let body = build_gemini_request(req);
        let headers = self
            .http
            .key
            .iter()
            .map(|k| ("x-goog-api-key".to_owned(), k.0.clone()))
            .collect();
        let (reply, header_id) = self.http.post(&self.endpoint, headers, &body)?;
        Ok(finish(parse_gemini_response(&reply)?, header_id, started))
    }
}

// ---------------------------------------------------------------------------------------------
// Construction from config
// ---------------------------------------------------------------------------------------------

/// Build the provider named by the `[provider]` section. `env` looks up environment variables
/// (pass `&|n| std::env::var(n).ok()`); only the NAME from the config is used to fetch a key.
///
/// A `mock` provider built here has no canned responses (every call fails); use
/// [`build_provider_with_mock_dir`] to load fixture responses.
pub fn build_provider(
    section: &ProviderSection,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn Provider>, ProviderError> {
    build_provider_with_mock_dir(section, env, None)
}

/// Like [`build_provider`], loading mock responses from `mock_dir` when the kind is `mock`.
pub fn build_provider_with_mock_dir(
    section: &ProviderSection,
    env: &dyn Fn(&str) -> Option<String>,
    mock_dir: Option<&Path>,
) -> Result<Box<dyn Provider>, ProviderError> {
    let key = |required: bool| -> Result<Option<String>, ProviderError> {
        match &section.api_key_env {
            Some(name) => match env(name).filter(|v| !v.trim().is_empty()) {
                Some(v) => Ok(Some(v)),
                None => Err(ProviderError::MissingApiKey {
                    provider: section.kind.name().to_owned(),
                    env_name: name.clone(),
                }),
            },
            None if required => Err(ProviderError::Config(format!(
                "provider.api_key_env is required for {}",
                section.kind.name()
            ))),
            None => Ok(None),
        }
    };
    match section.kind {
        ConfigProviderKind::Mock => {
            let mock = match mock_dir {
                Some(dir) => MockProvider::from_fixture_dir(dir)?,
                None => MockProvider::scripted(Vec::new()),
            };
            Ok(Box::new(mock.with_identity("mock", section.model.clone())))
        }
        ConfigProviderKind::OpenAi => {
            let key = key(true)?.unwrap_or_default();
            Ok(Box::new(OpenAiProvider::new(
                section.model.clone(),
                key,
                section.base_url.as_deref(),
            )?))
        }
        ConfigProviderKind::Gemini => {
            let key = key(true)?.unwrap_or_default();
            Ok(Box::new(GeminiProvider::new(
                section.model.clone(),
                key,
                section.base_url.as_deref(),
            )?))
        }
        ConfigProviderKind::Local => {
            let base = section.base_url.as_deref().ok_or_else(|| {
                ProviderError::Config("provider.base_url is required for local".to_owned())
            })?;
            Ok(Box::new(LocalProvider::new(
                section.model.clone(),
                base,
                key(false)?,
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", ApiKey("sk-secret-value".into())),
            "ApiKey([REDACTED])"
        );
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("http://localhost:8080/v1"));
        assert!(is_loopback("http://127.0.0.1/v1"));
        assert!(!is_loopback("http://10.0.0.5:8080/v1"));
    }

    #[test]
    fn input_estimate_rounds_up() {
        let req = CompletionRequest {
            purpose: "x".into(),
            system: "abc".into(),
            user: "de".into(),
            max_output_tokens: 1,
            temperature: None,
            top_p: None,
            seed: None,
            json_schema: None,
        };
        assert_eq!(req.estimated_input_tokens(), 2);
    }
}
