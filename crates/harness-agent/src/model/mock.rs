//! Scripted model client for tests and offline runs.
//!
//! [`MockModelClient`] answers requests from a queue of [`Scripted`] replies and records every
//! request it receives. It can be built in code or loaded from a fixture directory of numbered
//! JSON files (`01.json`, `02.json`, ...), mirroring core's `MockProvider::from_fixture_dir`.
//!
//! A fixture file looks like:
//!
//! ```json
//! {"text": "done", "tool_calls": [{"name": "read_file", "arguments": {"path": "a"}}],
//!  "usage": {"input": 100, "output_including_reasoning": 20}}
//! ```
//!
//! or `{"error": "message"}` to make that call fail.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    AssistantTurn, Item, ModelClient, ModelError, ModelRequest, ModelResponse, SendFuture,
    StreamEvent, TokenUsage, ToolCall,
};
use crate::registry::{Effort, ProviderId};

/// Usage reported by scripted replies that set none.
pub const DEFAULT_USAGE: TokenUsage = TokenUsage {
    input: Some(100),
    cached_input: None,
    cache_write: None,
    output_including_reasoning: Some(20),
    reasoning: None,
};

/// One scripted reply.
#[derive(Debug)]
pub enum Scripted {
    /// A normal model turn.
    Reply {
        /// Visible text.
        text: String,
        /// Tool calls as `(call_id, name, arguments)`.
        tool_calls: Vec<(String, String, Value)>,
        /// Reported usage.
        usage: TokenUsage,
    },
    /// The call fails with this error.
    Error(ModelError),
    /// The call never completes until it is cancelled, then returns [`ModelError::Cancelled`].
    Pending,
}

impl Scripted {
    /// Text-only reply.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Reply {
            text: text.into(),
            tool_calls: Vec::new(),
            usage: DEFAULT_USAGE,
        }
    }

    /// Reply with tool calls (`(name, arguments)`); call ids are generated as `mock_call_<n>`.
    pub fn tool_calls(text: impl Into<String>, calls: &[(&str, Value)]) -> Self {
        Self::Reply {
            text: text.into(),
            tool_calls: calls
                .iter()
                .enumerate()
                .map(|(i, (name, args))| {
                    (format!("mock_call_{i}"), (*name).to_owned(), args.clone())
                })
                .collect(),
            usage: DEFAULT_USAGE,
        }
    }

    /// Failing call.
    pub fn error(error: ModelError) -> Self {
        Self::Error(error)
    }

    /// Override the usage of a reply (no effect on other variants).
    #[must_use]
    pub fn with_usage(mut self, new_usage: TokenUsage) -> Self {
        if let Self::Reply { usage, .. } = &mut self {
            *usage = new_usage;
        }
        self
    }
}

/// A request as the mock saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    /// Alias of the requested model.
    pub model_alias: String,
    /// Requested effort.
    pub effort: Effort,
    /// System instructions.
    pub instructions: String,
    /// Conversation history at call time.
    pub history: Vec<Item>,
    /// Names of the tools offered.
    pub tool_names: Vec<String>,
    /// Output token limit.
    pub max_output_tokens: u32,
    /// Structured-output schema, when set.
    pub json_schema: Option<Value>,
}

/// Scripted [`ModelClient`].
#[derive(Debug)]
pub struct MockModelClient {
    provider: ProviderId,
    queue: Mutex<VecDeque<Scripted>>,
    recorded: Mutex<Vec<RecordedRequest>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureCall {
    name: String,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureStep {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_calls: Vec<FixtureCall>,
    #[serde(default)]
    usage: Option<TokenUsage>,
    #[serde(default)]
    error: Option<String>,
}

impl MockModelClient {
    /// Client answering with `script` in order. Once the script is exhausted every call fails
    /// with [`ModelError::Mock`].
    pub fn new(provider: ProviderId, script: Vec<Scripted>) -> Self {
        Self {
            provider,
            queue: Mutex::new(script.into()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// Load `*.json` fixture files from `dir`, answered in file-name order.
    pub fn from_fixture_dir(
        provider: ProviderId,
        dir: impl AsRef<Path>,
    ) -> Result<Self, ModelError> {
        let dir = dir.as_ref();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| ModelError::Mock(format!("cannot read {}: {e}", dir.display())))?;
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Err(ModelError::Mock(format!(
                "no *.json files in {}",
                dir.display()
            )));
        }
        let mut script = Vec::new();
        for path in paths {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ModelError::Mock(format!("cannot read {}: {e}", path.display())))?;
            let step: FixtureStep = serde_json::from_str(&text)
                .map_err(|e| ModelError::Mock(format!("invalid {}: {e}", path.display())))?;
            if let Some(message) = step.error {
                script.push(Scripted::Error(ModelError::Mock(message)));
                continue;
            }
            script.push(Scripted::Reply {
                text: step.text.unwrap_or_default(),
                tool_calls: step
                    .tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| {
                        (
                            c.call_id.unwrap_or_else(|| format!("mock_call_{i}")),
                            c.name,
                            c.arguments.unwrap_or_else(|| json!({})),
                        )
                    })
                    .collect(),
                usage: step.usage.unwrap_or(DEFAULT_USAGE),
            });
        }
        Ok(Self::new(provider, script))
    }

    /// Requests received so far (including ones that failed).
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Number of `send` calls received.
    pub fn call_count(&self) -> usize {
        self.recorded.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Replies still queued.
    pub fn remaining(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }
}

impl ModelClient for MockModelClient {
    fn provider(&self) -> ProviderId {
        self.provider
    }

    fn send<'a>(
        &'a self,
        req: ModelRequest<'a>,
        stream: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> SendFuture<'a> {
        let recorded = RecordedRequest {
            model_alias: req.spec.alias.clone(),
            effort: req.effort,
            instructions: req.instructions.to_owned(),
            history: req.history.to_vec(),
            tool_names: req.tools.iter().map(|t| t.name.clone()).collect(),
            max_output_tokens: req.max_output_tokens,
            json_schema: req.json_schema.cloned(),
        };
        if let Ok(mut log) = self.recorded.lock() {
            log.push(recorded);
        }
        let next = self.queue.lock().ok().and_then(|mut q| q.pop_front());
        let alias = req.spec.alias.clone();
        Box::pin(async move {
            match next {
                None => Err(ModelError::Mock("script exhausted".to_owned())),
                Some(Scripted::Error(error)) => Err(error),
                Some(Scripted::Pending) => {
                    cancel.cancelled().await;
                    Err(ModelError::Cancelled)
                }
                Some(Scripted::Reply {
                    text,
                    tool_calls,
                    usage,
                }) => {
                    if cancel.is_cancelled() {
                        return Err(ModelError::Cancelled);
                    }
                    let mut raw = Vec::new();
                    if !text.is_empty() {
                        let _ = stream.send(StreamEvent::TextDelta(text.clone()));
                        raw.push(json!({"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": text}]}));
                    }
                    let calls: Vec<ToolCall> = tool_calls
                        .into_iter()
                        .map(|(call_id, name, arguments)| {
                            let _ = stream.send(StreamEvent::ToolCallStarted {
                                call_id: call_id.clone(),
                                name: name.clone(),
                            });
                            let raw_arguments = arguments.to_string();
                            raw.push(json!({"type": "function_call", "call_id": call_id,
                                "name": name, "arguments": raw_arguments}));
                            ToolCall {
                                call_id,
                                name,
                                arguments,
                                raw_arguments,
                            }
                        })
                        .collect();
                    Ok(ModelResponse {
                        turn: AssistantTurn {
                            model_alias: alias,
                            text,
                            tool_calls: calls,
                            raw,
                        },
                        usage,
                        request_id: "mock-request".to_owned(),
                        latency_ms: 1,
                    })
                }
            }
        })
    }
}
