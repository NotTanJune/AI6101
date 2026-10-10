//! Provider-neutral conversation types and the [`ModelClient`] trait.
//!
//! The agent loop speaks only these types. Each provider client maps them onto its own wire
//! format (OpenAI Responses items, Gemini Interactions steps) and maps the reply back. An
//! [`AssistantTurn`] keeps the provider's output exactly as received in `raw`, so encrypted
//! reasoning and thought signatures can be replayed to the model that produced them.

pub mod common;
pub mod gemini;
pub mod mock;
pub mod openai;
pub mod sse;

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::registry::{Effort, ModelSpec, ProviderId};

/// One entry of the conversation history.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// User message.
    User {
        /// Message text.
        text: String,
    },
    /// Model turn.
    Assistant(AssistantTurn),
    /// Result of a tool call.
    ToolResult {
        /// Call this result answers.
        call_id: String,
        /// Name of the tool that ran.
        name: String,
        /// Output text fed back to the model.
        output: String,
        /// Whether the tool failed.
        is_error: bool,
    },
}

/// One model turn: visible text, tool calls and the provider's raw output.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTurn {
    /// Alias of the model that produced the turn.
    pub model_alias: String,
    /// Concatenated visible text.
    pub text: String,
    /// Tool calls requested by the model, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Provider output items or steps exactly as received.
    pub raw: Vec<Value>,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id, echoed in the matching [`Item::ToolResult`].
    pub call_id: String,
    /// Exposed tool name.
    pub name: String,
    /// Parsed arguments. Stays a JSON string when the model sent invalid JSON; the tool
    /// layer reports that as a tool error.
    pub arguments: Value,
    /// Arguments as the provider sent them.
    pub raw_arguments: String,
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    /// Exposed name.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

/// One model request.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    /// Model to call.
    pub spec: &'a ModelSpec,
    /// Reasoning effort.
    pub effort: Effort,
    /// System instructions.
    pub instructions: &'a str,
    /// Conversation so far.
    pub history: &'a [Item],
    /// Tools offered.
    pub tools: &'a [ToolSchema],
    /// Output token limit (includes reasoning).
    pub max_output_tokens: u32,
    /// JSON Schema for structured output (used by the core bridge only).
    pub json_schema: Option<&'a Value>,
}

/// Token usage of one call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens.
    pub input: Option<u64>,
    /// Cached input tokens (subset of `input`).
    pub cached_input: Option<u64>,
    /// Cache-write input tokens (subset of `input`).
    pub cache_write: Option<u64>,
    /// Output tokens including reasoning tokens.
    pub output_including_reasoning: Option<u64>,
    /// Reasoning tokens (informational).
    pub reasoning: Option<u64>,
}

/// Result of one model call.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    /// The model's turn.
    pub turn: AssistantTurn,
    /// Token usage.
    pub usage: TokenUsage,
    /// Provider request id, or `unavailable`.
    pub request_id: String,
    /// Wall-clock latency of the call.
    pub latency_ms: u64,
}

/// Incremental output forwarded while a call is running.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A piece of visible text.
    TextDelta(String),
    /// The model started a tool call.
    ToolCallStarted {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
    },
}

/// Model call failure. Messages never contain the API key.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The environment variable that should hold the API key is unset or empty.
    #[error("environment variable {env_name} is not set; cannot authenticate to {provider}")]
    MissingApiKey {
        /// Provider name.
        provider: String,
        /// Variable name (never its value).
        env_name: String,
    },
    /// Client configuration problem.
    #[error("model client configuration: {0}")]
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
    /// The server answered but the reply is not a usable model turn.
    #[error("unusable model response: {0}")]
    InvalidResponse(String),
    /// Mock client has nothing to return.
    #[error("mock model: {0}")]
    Mock(String),
    /// The call was cancelled.
    #[error("model call cancelled")]
    Cancelled,
    /// The model does not accept the requested effort.
    #[error("model {model} does not support effort {effort}")]
    UnsupportedEffort {
        /// Model alias.
        model: String,
        /// Requested effort.
        effort: Effort,
    },
}

impl ModelError {
    /// Whether the provider may have processed (and billed) the request even though no usage
    /// came back. Same rule as core's `ProviderError::billing_uncertain`: timeouts, server
    /// errors (status 500 and above) and unusable success responses.
    pub fn billing_uncertain(&self) -> bool {
        match self {
            Self::Timeout | Self::InvalidResponse(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// Boxed future returned by [`ModelClient::send`].
pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelResponse, ModelError>> + Send + 'a>>;

/// A model provider client.
///
/// Implementations forward incremental output on `stream` (the receiver may be dropped; sends
/// are best effort) and must return [`ModelError::Cancelled`] promptly when `cancel` fires.
pub trait ModelClient: Send + Sync {
    /// Provider this client talks to.
    fn provider(&self) -> ProviderId;

    /// Run one request.
    fn send<'a>(
        &'a self,
        req: ModelRequest<'a>,
        stream: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> SendFuture<'a>;
}

// ---------------------------------------------------------------------------------------------
// History replay
// ---------------------------------------------------------------------------------------------

/// A tool result as seen by the replay helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultRef<'a> {
    /// Call this result answers.
    pub call_id: &'a str,
    /// Name of the tool that ran.
    pub name: &'a str,
    /// Output text.
    pub output: &'a str,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// One history entry classified for replay to a given model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Replayed<'a> {
    /// User message.
    User(&'a str),
    /// Turn produced by the same model alias: replay `raw` verbatim.
    Own(&'a AssistantTurn),
    /// Turn produced by another model: render from `text` and `tool_calls`.
    Foreign(&'a AssistantTurn),
    /// Tool result. `foreign` is true when it answers a call made by a foreign turn.
    ToolResult {
        /// The result.
        result: ToolResultRef<'a>,
        /// Whether the call it answers came from a foreign turn.
        foreign: bool,
    },
}

/// Classify `history` for replay to the model with alias `alias`.
///
/// An [`AssistantTurn`] is replayed from `raw` only when `turn.model_alias == alias`. Any other
/// turn is foreign, which keeps encrypted reasoning and thought signatures with the model that
/// produced them. A tool result is foreign when its `call_id` was issued by a foreign turn.
pub fn classify_history<'a>(history: &'a [Item], alias: &str) -> Vec<Replayed<'a>> {
    let mut foreign_calls: Vec<&str> = Vec::new();
    let mut out = Vec::with_capacity(history.len());
    for item in history {
        out.push(match item {
            Item::User { text } => Replayed::User(text),
            Item::Assistant(turn) if turn.model_alias == alias => Replayed::Own(turn),
            Item::Assistant(turn) => {
                foreign_calls.extend(turn.tool_calls.iter().map(|c| c.call_id.as_str()));
                Replayed::Foreign(turn)
            }
            Item::ToolResult {
                call_id,
                name,
                output,
                is_error,
            } => Replayed::ToolResult {
                result: ToolResultRef {
                    call_id,
                    name,
                    output,
                    is_error: *is_error,
                },
                foreign: foreign_calls.contains(&call_id.as_str()),
            },
        });
    }
    out
}
