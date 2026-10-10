//! Append-only JSONL session transcript.
//!
//! Each line is `{"schema":"agent-session/1","ts":<RFC 3339>,"kind":<kind>,...}` and is
//! described by `schemas/agent-session.schema.json`. Every string passes through the core
//! [`Redactor`] before it is written, so API keys never reach disk.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use harness_core::redact::Redactor;
use serde_json::{Map, Value, json};

use crate::approval::{ApprovalClass, Decision};
use crate::events::StopReason;
use crate::model::TokenUsage;
use crate::registry::Effort;

/// Schema version string of every line.
pub const TRANSCRIPT_SCHEMA_VERSION: &str = "agent-session/1";

/// The line schema, embedded at compile time.
pub const TRANSCRIPT_SCHEMA_JSON: &str = include_str!("../../../schemas/agent-session.schema.json");

/// File name inside the session directory.
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// Transcript failure.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// The file could not be created or written.
    #[error("transcript {path}: {message}")]
    Io {
        /// File path.
        path: PathBuf,
        /// OS error text.
        message: String,
    },
}

/// Something worth recording.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEvent {
    /// The session started.
    SessionStart {
        /// Model alias.
        model_alias: String,
        /// Effort.
        effort: Effort,
    },
    /// The user switched model or effort.
    ModelSwitch {
        /// New model alias.
        model_alias: String,
        /// New effort.
        effort: Effort,
    },
    /// A user message.
    UserMessage {
        /// Text.
        text: String,
    },
    /// A model turn.
    AssistantMessage {
        /// Model alias.
        model_alias: String,
        /// Visible text.
        text: String,
        /// Number of tool calls requested.
        tool_call_count: usize,
    },
    /// A tool call.
    ToolCall {
        /// Call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Arguments.
        arguments: Value,
    },
    /// A tool result.
    ToolResult {
        /// Call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Output text.
        output: String,
        /// Whether the call failed.
        is_error: bool,
        /// Duration.
        duration_ms: u64,
    },
    /// An approval decision.
    Approval {
        /// Request id.
        id: u64,
        /// Tool name.
        tool: String,
        /// Class.
        class: ApprovalClass,
        /// Summary shown to the user.
        summary: String,
        /// The answer.
        decision: Decision,
    },
    /// Usage and cost of one model call.
    Usage {
        /// Model alias.
        model_alias: String,
        /// Token usage.
        usage: TokenUsage,
        /// USD, when known.
        cost_usd: Option<f64>,
    },
    /// Drift between the Tool KB and a live server.
    Drift {
        /// Server name.
        server: String,
        /// KB cards of that server.
        kb_cards: usize,
        /// Live tools.
        live_tools: usize,
        /// Number of detected changes.
        change_count: usize,
    },
    /// Informational message.
    Notice {
        /// Text.
        text: String,
    },
    /// Error message.
    Error {
        /// Text.
        text: String,
    },
    /// A turn ended.
    TurnFinished {
        /// Reason.
        reason: StopReason,
    },
}

impl TranscriptEvent {
    /// The `kind` field of the line.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "session_start",
            Self::ModelSwitch { .. } => "model_switch",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Approval { .. } => "approval",
            Self::Usage { .. } => "usage",
            Self::Drift { .. } => "drift",
            Self::Notice { .. } => "notice",
            Self::Error { .. } => "error",
            Self::TurnFinished { .. } => "turn_finished",
        }
    }

    /// The kind-specific fields as a JSON object (before redaction).
    fn fields(&self) -> Map<String, Value> {
        let value = match self {
            Self::SessionStart {
                model_alias,
                effort,
            }
            | Self::ModelSwitch {
                model_alias,
                effort,
            } => json!({"model_alias": model_alias, "effort": effort.as_str()}),
            Self::UserMessage { text } | Self::Notice { text } | Self::Error { text } => {
                json!({"text": text})
            }
            Self::AssistantMessage {
                model_alias,
                text,
                tool_call_count,
            } => {
                json!({"model_alias": model_alias, "text": text, "tool_call_count": tool_call_count})
            }
            Self::ToolCall {
                call_id,
                name,
                arguments,
            } => json!({"call_id": call_id, "name": name, "arguments": arguments}),
            Self::ToolResult {
                call_id,
                name,
                output,
                is_error,
                duration_ms,
            } => json!({"call_id": call_id, "name": name, "output": output,
                        "is_error": is_error, "duration_ms": duration_ms}),
            Self::Approval {
                id,
                tool,
                class,
                summary,
                decision,
            } => json!({"id": id, "tool": tool, "class": class.as_str(), "summary": summary,
            "decision": match decision {
                Decision::Once => "once",
                Decision::Session => "session",
                Decision::Deny => "deny",
            }}),
            Self::Usage {
                model_alias,
                usage,
                cost_usd,
            } => json!({"model_alias": model_alias, "input_tokens": usage.input,
                        "output_tokens": usage.output_including_reasoning,
                        "cached_input_tokens": usage.cached_input, "cost_usd": cost_usd}),
            Self::Drift {
                server,
                kb_cards,
                live_tools,
                change_count,
            } => json!({"server": server, "kb_cards": kb_cards, "live_tools": live_tools,
                        "change_count": change_count}),
            Self::TurnFinished { reason } => {
                let detail = match reason {
                    StopReason::Error(message) => Value::String(message.clone()),
                    _ => Value::Null,
                };
                json!({"reason": reason.as_str(), "detail": detail})
            }
        };
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }
}

/// Writes transcript lines.
#[derive(Debug)]
pub struct TranscriptWriter {
    file: File,
    path: PathBuf,
    redactor: Redactor,
}

impl TranscriptWriter {
    /// Open `<session_dir>/transcript.jsonl` for appending, creating the directory.
    pub fn create(session_dir: &Path, redactor: Redactor) -> Result<Self, TranscriptError> {
        let path = session_dir.join(TRANSCRIPT_FILE);
        let io_error = |e: std::io::Error| TranscriptError::Io {
            path: path.clone(),
            message: e.to_string(),
        };
        std::fs::create_dir_all(session_dir).map_err(io_error)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        Ok(Self {
            file,
            path,
            redactor,
        })
    }

    /// Path of the transcript file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Build the redacted line for an event (without writing it).
    pub fn line(&self, event: &TranscriptEvent) -> Value {
        let mut object = Map::new();
        object.insert("schema".to_owned(), json!(TRANSCRIPT_SCHEMA_VERSION));
        object.insert(
            "ts".to_owned(),
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        object.insert("kind".to_owned(), json!(event.kind()));
        object.extend(event.fields());
        let mut value = Value::Object(object);
        self.redactor.redact_json(&mut value);
        value
    }

    /// Append one event as a JSONL line.
    pub fn record(&mut self, event: &TranscriptEvent) -> Result<(), TranscriptError> {
        let mut text = self.line(event).to_string();
        text.push('\n');
        self.file
            .write_all(text.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|e| TranscriptError::Io {
                path: self.path.clone(),
                message: e.to_string(),
            })
    }
}
