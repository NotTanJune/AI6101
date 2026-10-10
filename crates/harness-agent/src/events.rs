//! Events the agent loop sends to its front end.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::approval::{ApprovalRequest, Decision};
use crate::drift::DriftReport;
use crate::model::TokenUsage;

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished: no tool calls (interactive) or the finish marker (benchmark).
    Completed,
    /// The step limit was reached.
    MaxSteps,
    /// The next model call would exceed the budget cap.
    Budget,
    /// The user cancelled the turn.
    Cancelled,
    /// Benchmark policy only: the model stopped calling tools without the finish marker, or
    /// produced neither text nor tool calls.
    Stalled,
    /// A model call failed.
    Error(String),
}

impl StopReason {
    /// Stable lowercase name used in the transcript.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::MaxSteps => "max_steps",
            Self::Budget => "budget",
            Self::Cancelled => "cancelled",
            Self::Stalled => "stalled",
            Self::Error(_) => "error",
        }
    }
}

/// One event of a running turn.
#[derive(Debug)]
pub enum AgentEvent {
    /// A turn began.
    TurnStarted,
    /// A piece of visible model text.
    TextDelta(String),
    /// The model called a tool.
    ToolCallStarted {
        /// Provider call id.
        call_id: String,
        /// Exposed tool name.
        name: String,
        /// Parsed arguments.
        arguments: Value,
    },
    /// A tool call finished (including denied and failed calls).
    ToolCallFinished {
        /// Provider call id.
        call_id: String,
        /// Output text fed back to the model.
        output: String,
        /// Whether the call failed.
        is_error: bool,
        /// Wall-clock duration.
        duration_ms: u64,
    },
    /// The loop waits for the user. Answer through the sender; dropping it denies the call.
    ApprovalRequested(ApprovalRequest, oneshot::Sender<Decision>),
    /// Usage and cost of one model call.
    Usage {
        /// Model that answered.
        model_alias: String,
        /// Token usage.
        usage: TokenUsage,
        /// USD charged by the rate card, `None` when unknown.
        cost_usd: Option<f64>,
    },
    /// An MCP server's live tools differ from the Tool KB.
    Drift(DriftReport),
    /// Informational message.
    Notice(String),
    /// A failure worth showing.
    Error(String),
    /// The turn ended.
    TurnFinished {
        /// Why it ended.
        reason: StopReason,
    },
}
