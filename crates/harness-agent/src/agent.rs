//! The agent loop.
//!
//! [`Agent::run_turn`] appends the user message and then loops: reserve budget, call the model,
//! settle usage, append the assistant turn, run its tool calls one after another (asking the
//! user first when the approval policy says so) and append the results. The loop ends when the
//! model stops calling tools (or the benchmark stop policy says so), the step limit is hit, the
//! budget refuses the next call, or the turn is cancelled.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use harness_core::cost::{CostError, CostLedger, ProviderKind, ReserveRequest, Usage};
use harness_core::events::Phase;
use harness_core::model::ToolKb;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::approval::{ApprovalClass, ApprovalPolicy, ApprovalRequest, DENIED_TEXT, Decision};
use crate::drift::{DriftReport, detect_drift};
use crate::events::AgentEvent;
pub use crate::events::StopReason;
use crate::mcp::{LiveTool, McpConnection};
use crate::model::{
    AssistantTurn, Item, ModelClient, ModelError, ModelRequest, StreamEvent, ToolCall,
};
use crate::registry::{Effort, ModelRegistry, ModelSpec, ProviderId, RegistryError};
use crate::tools::builtin::{BuiltinTools, ToolOutput, approval_class};
use crate::tools::{ToolRegistry, ToolSource};
use crate::transcript::{TranscriptEvent, TranscriptWriter};

/// When the loop stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopPolicy {
    /// Stop when an assistant turn has no tool calls.
    Interactive,
    /// Benchmark mechanics: stop when the text contains `finish_marker`. A turn with text and no
    /// tool calls gets a user message with `nudge_text` appended, until `max_no_tool_turns` such
    /// turns have happened; then the turn stops as [`StopReason::Stalled`]. A turn with neither
    /// text nor tool calls stops as [`StopReason::Stalled`] at once.
    Benchmark {
        /// Text that marks the task as finished.
        finish_marker: String,
        /// User message appended after a turn without tool calls.
        nudge_text: String,
        /// Number of turns without tool calls that ends the run.
        max_no_tool_turns: u32,
    },
}

/// Agent settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Alias of the model to use.
    pub model_alias: String,
    /// Reasoning effort.
    pub effort: Effort,
    /// System instructions.
    pub instructions: String,
    /// Most model calls per turn.
    pub max_steps: u32,
    /// Cut tool results to this many characters before the model sees them.
    pub tool_result_max_chars: Option<usize>,
    /// Whether the built-in coding tools are offered and executable.
    pub builtins_enabled: bool,
    /// Stop policy.
    pub stop_policy: StopPolicy,
    /// Output token limit per model call (clamped to the model's maximum). It also sizes the
    /// budget reservation, so keep it modest.
    pub max_output_tokens: u32,
}

impl AgentConfig {
    /// Defaults: 50 steps, no result truncation, built-ins on, interactive stop policy and a
    /// 32,000 token output limit.
    pub fn new(
        model_alias: impl Into<String>,
        effort: Effort,
        instructions: impl Into<String>,
    ) -> Self {
        Self {
            model_alias: model_alias.into(),
            effort,
            instructions: instructions.into(),
            max_steps: 50,
            tool_result_max_chars: None,
            builtins_enabled: true,
            stop_policy: StopPolicy::Interactive,
            max_output_tokens: 32_000,
        }
    }
}

/// Agent construction failure.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Model alias or effort is not valid.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The model does not accept the effort.
    #[error("model {model} does not support effort {effort}")]
    UnsupportedEffort {
        /// Model alias.
        model: String,
        /// Requested effort.
        effort: Effort,
    },
    /// Built-in tools are enabled but no workspace was given, or they clash with MCP names.
    #[error("tool setup: {0}")]
    Tools(String),
}

/// Everything an [`Agent`] needs besides its config.
pub struct AgentParts {
    /// Known models.
    pub models: ModelRegistry,
    /// One client per provider.
    pub clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
    /// Tools offered to the model (MCP tools already added). Built-ins are added or removed
    /// according to [`AgentConfig::builtins_enabled`].
    pub tools: ToolRegistry,
    /// Executor for built-in tools; required when built-ins are enabled.
    pub builtins: Option<BuiltinTools>,
    /// Shared budget ledger.
    pub ledger: Arc<Mutex<CostLedger>>,
    /// Approval policy.
    pub policy: ApprovalPolicy,
    /// Session transcript, when recording.
    pub transcript: Option<TranscriptWriter>,
}

/// The agent.
pub struct Agent {
    config: AgentConfig,
    models: ModelRegistry,
    clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
    tools: ToolRegistry,
    builtins: Option<BuiltinTools>,
    mcp: HashMap<String, Arc<McpConnection>>,
    ledger: Arc<Mutex<CostLedger>>,
    policy: ApprovalPolicy,
    transcript: Option<TranscriptWriter>,
    history: Vec<Item>,
    next_approval_id: u64,
    transcript_error: Option<String>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("model_alias", &self.config.model_alias)
            .field("history_len", &self.history.len())
            .finish_non_exhaustive()
    }
}

/// Estimate of the input tokens of a request: characters divided by four, rounded up.
fn estimate_input_tokens(
    instructions: &str,
    history: &[Item],
    tools: &[crate::model::ToolSchema],
) -> u64 {
    let mut chars = instructions.chars().count();
    for item in history {
        chars += match item {
            Item::User { text } => text.chars().count(),
            Item::Assistant(turn) => turn.raw.iter().map(|v| v.to_string().chars().count()).sum(),
            Item::ToolResult { output, .. } => output.chars().count(),
        };
    }
    for tool in tools {
        chars += tool.name.chars().count()
            + tool.description.chars().count()
            + tool.parameters.to_string().chars().count();
    }
    (chars as u64).div_ceil(4)
}

fn truncate_output(text: String, max_chars: Option<usize>) -> String {
    let Some(max) = max_chars else {
        return text;
    };
    let total = text.chars().count();
    if total <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push_str(&format!("\n[truncated {} characters]", total - max));
    cut
}

fn lock_ledger(ledger: &Mutex<CostLedger>) -> std::sync::MutexGuard<'_, CostLedger> {
    ledger.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Agent {
    /// Build an agent. Validates the model alias and effort against the registry.
    pub fn new(config: AgentConfig, parts: AgentParts) -> Result<Self, AgentError> {
        let spec = parts.models.get(&config.model_alias)?;
        if !spec.effort_supported(config.effort) {
            return Err(AgentError::UnsupportedEffort {
                model: config.model_alias.clone(),
                effort: config.effort,
            });
        }
        let mut tools = parts.tools;
        let mut builtins = parts.builtins;
        // The agent, not the caller, guarantees that shell commands never see model API keys:
        // every `api_key_env` named in the registry is scrubbed, whatever the caller passed.
        if let Some(builtins) = builtins.as_mut() {
            builtins.add_scrub_env(parts.models.api_key_envs());
        }
        if config.builtins_enabled {
            if builtins.is_none() {
                return Err(AgentError::Tools(
                    "built-in tools are enabled but no workspace was provided".to_owned(),
                ));
            }
            if !tools.has_builtins() {
                tools
                    .add_builtins()
                    .map_err(|e| AgentError::Tools(e.to_string()))?;
            }
        } else {
            tools.remove_builtins();
        }
        Ok(Self {
            config,
            models: parts.models,
            clients: parts.clients,
            tools,
            builtins,
            mcp: HashMap::new(),
            ledger: parts.ledger,
            policy: parts.policy,
            transcript: parts.transcript,
            history: Vec::new(),
            next_approval_id: 1,
            transcript_error: None,
        })
    }

    /// Current settings.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Conversation so far.
    pub fn history(&self) -> &[Item] {
        &self.history
    }

    /// Forget the conversation.
    pub fn reset(&mut self) {
        self.history.clear();
    }

    /// Tools offered to the model.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Mutable access to the tool registry (for example to expose an accepted KB).
    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }

    /// The built-in tool executor, when one is configured.
    pub fn builtins(&self) -> Option<&BuiltinTools> {
        self.builtins.as_ref()
    }

    /// Approval policy.
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }

    /// Shared ledger.
    pub fn ledger(&self) -> &Arc<Mutex<CostLedger>> {
        &self.ledger
    }

    /// Path of the durable session transcript, when recording is enabled.
    pub fn transcript_path(&self) -> Option<&std::path::Path> {
        self.transcript.as_ref().map(TranscriptWriter::path)
    }

    /// Record a runtime notice alongside the conversation without changing model history.
    pub fn record_notice(&mut self, text: impl Into<String>) {
        self.record(&TranscriptEvent::Notice { text: text.into() });
    }

    /// Register a connected MCP server so its tools can be called.
    pub fn add_mcp_connection(&mut self, name: impl Into<String>, connection: Arc<McpConnection>) {
        self.mcp.insert(name.into(), connection);
    }

    /// Change model and effort. Validated against the registry. Takes `&mut self`, so it cannot
    /// run while a turn is in progress. The history is kept: turns from another model are
    /// replayed as plain text and tool calls.
    pub fn set_model(&mut self, alias: &str, effort: Effort) -> Result<(), AgentError> {
        let spec = self.models.get(alias)?;
        if !spec.effort_supported(effort) {
            return Err(AgentError::UnsupportedEffort {
                model: alias.to_owned(),
                effort,
            });
        }
        self.config.model_alias = alias.to_owned();
        self.config.effort = effort;
        self.record(&TranscriptEvent::ModelSwitch {
            model_alias: alias.to_owned(),
            effort,
        });
        Ok(())
    }

    /// Record the session start in the transcript.
    pub fn record_session_start(&mut self) {
        let event = TranscriptEvent::SessionStart {
            model_alias: self.config.model_alias.clone(),
            effort: self.config.effort,
        };
        self.record(&event);
    }

    /// Compare the KB with a live server's tools, emit [`AgentEvent::Drift`] and record it.
    /// Returns the report so the caller can offer an adaptation.
    pub fn report_drift(
        &mut self,
        server: &str,
        kb: &ToolKb,
        live: &[LiveTool],
        events: &UnboundedSender<AgentEvent>,
    ) -> DriftReport {
        let (report, _live_kb) = detect_drift(server, kb, live);
        self.record(&TranscriptEvent::Drift {
            server: report.server.clone(),
            kb_cards: report.kb_cards,
            live_tools: report.live_tools,
            change_count: report.diff.changes.len(),
        });
        let _ = events.send(AgentEvent::Drift(report.clone()));
        report
    }

    fn record(&mut self, event: &TranscriptEvent) {
        if let Some(writer) = self.transcript.as_mut()
            && let Err(e) = writer.record(event)
        {
            self.transcript_error = Some(e.to_string());
        }
    }

    fn finish(&mut self, events: &UnboundedSender<AgentEvent>, reason: StopReason) -> StopReason {
        self.record(&TranscriptEvent::TurnFinished {
            reason: reason.clone(),
        });
        if let Some(message) = self.transcript_error.take() {
            let _ = events.send(AgentEvent::Notice(format!(
                "transcript write failed: {message}"
            )));
        }
        let _ = events.send(AgentEvent::TurnFinished {
            reason: reason.clone(),
        });
        reason
    }

    fn fail(&mut self, events: &UnboundedSender<AgentEvent>, message: String) -> StopReason {
        self.record(&TranscriptEvent::Error {
            text: message.clone(),
        });
        let _ = events.send(AgentEvent::Error(message.clone()));
        self.finish(events, StopReason::Error(message))
    }

    /// Run one user turn to completion. Always ends with [`AgentEvent::TurnFinished`] and
    /// returns the same reason.
    pub async fn run_turn(
        &mut self,
        user_text: &str,
        events: UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> StopReason {
        let _ = events.send(AgentEvent::TurnStarted);
        self.history.push(Item::User {
            text: user_text.to_owned(),
        });
        self.record(&TranscriptEvent::UserMessage {
            text: user_text.to_owned(),
        });
        let mut no_tool_turns: u32 = 0;
        for _step in 0..self.config.max_steps {
            if cancel.is_cancelled() {
                return self.finish(&events, StopReason::Cancelled);
            }
            let turn = match self.model_call(&events, &cancel).await {
                Ok(turn) => turn,
                Err(reason) => return reason,
            };
            let text = turn.text.clone();
            let calls = turn.tool_calls.clone();
            self.record(&TranscriptEvent::AssistantMessage {
                model_alias: turn.model_alias.clone(),
                text: turn.text.clone(),
                tool_call_count: calls.len(),
            });
            self.history.push(Item::Assistant(turn));

            let marker_hit = match &self.config.stop_policy {
                StopPolicy::Benchmark { finish_marker, .. } => {
                    text.contains(finish_marker.as_str())
                }
                StopPolicy::Interactive => false,
            };

            if calls.is_empty() {
                match self.config.stop_policy.clone() {
                    StopPolicy::Interactive => {
                        return self.finish(&events, StopReason::Completed);
                    }
                    StopPolicy::Benchmark {
                        nudge_text,
                        max_no_tool_turns,
                        ..
                    } => {
                        if marker_hit {
                            return self.finish(&events, StopReason::Completed);
                        }
                        if text.is_empty() {
                            return self.finish(&events, StopReason::Stalled);
                        }
                        no_tool_turns += 1;
                        if no_tool_turns >= max_no_tool_turns {
                            return self.finish(&events, StopReason::Stalled);
                        }
                        self.history.push(Item::User {
                            text: nudge_text.clone(),
                        });
                        self.record(&TranscriptEvent::UserMessage { text: nudge_text });
                        continue;
                    }
                }
            }

            if self.run_tool_calls(&calls, &events, &cancel).await {
                return self.finish(&events, StopReason::Cancelled);
            }
            if marker_hit {
                return self.finish(&events, StopReason::Completed);
            }
        }
        self.finish(&events, StopReason::MaxSteps)
    }

    /// One model call with budget handling. `Err` carries the finished stop reason.
    async fn model_call(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn, StopReason> {
        let spec: ModelSpec = match self.models.get(&self.config.model_alias) {
            Ok(spec) => spec.clone(),
            Err(e) => return Err(self.fail(events, e.to_string())),
        };
        let Some(client) = self.clients.get(&spec.provider).cloned() else {
            let message = format!(
                "no client configured for provider {}",
                spec.provider.as_str()
            );
            return Err(self.fail(events, message));
        };
        let schemas = self.tools.to_schemas();
        let max_output_tokens = self.config.max_output_tokens.min(spec.max_output_tokens);
        let estimate = estimate_input_tokens(&self.config.instructions, &self.history, &schemas);
        let reservation = lock_ledger(&self.ledger).reserve(ReserveRequest {
            provider: spec.provider.as_str().to_owned(),
            model: spec.model_id.clone(),
            phase: Phase::Agent,
            input_tokens_estimate: estimate,
            max_output_tokens,
            kind: ProviderKind::Paid,
        });
        let reservation = match reservation {
            Ok(r) => r,
            Err(e @ CostError::CapExceeded { .. }) => {
                let message = e.to_string();
                self.record(&TranscriptEvent::Notice {
                    text: message.clone(),
                });
                let _ = events.send(AgentEvent::Notice(message));
                return Err(self.finish(events, StopReason::Budget));
            }
            Err(e) => return Err(self.fail(events, e.to_string())),
        };

        let (tx, mut rx) = unbounded_channel::<StreamEvent>();
        let request = ModelRequest {
            spec: &spec,
            effort: self.config.effort,
            instructions: &self.config.instructions,
            history: &self.history,
            tools: &schemas,
            max_output_tokens,
            json_schema: None,
        };
        let result = {
            let send = client.send(request, tx, cancel.clone());
            tokio::pin!(send);
            let forward = |event: StreamEvent| {
                if let StreamEvent::TextDelta(delta) = event {
                    let _ = events.send(AgentEvent::TextDelta(delta));
                }
            };
            loop {
                tokio::select! {
                    biased;
                    Some(event) = rx.recv() => forward(event),
                    result = &mut send => {
                        while let Ok(event) = rx.try_recv() {
                            forward(event);
                        }
                        break result;
                    }
                }
            }
        };

        match result {
            Ok(response) => {
                let usage = Usage {
                    request_id: response.request_id.clone(),
                    input_tokens: response.usage.input,
                    output_tokens: response.usage.output_including_reasoning,
                    cached_input_tokens: response.usage.cached_input,
                    cache_write_tokens: response.usage.cache_write,
                    reasoning_tokens: response.usage.reasoning,
                    latency_ms: response.latency_ms,
                };
                let record = lock_ledger(&self.ledger).settle(reservation, usage);
                self.record(&TranscriptEvent::Usage {
                    model_alias: spec.alias.clone(),
                    usage: response.usage,
                    cost_usd: record.estimated_usd,
                });
                let _ = events.send(AgentEvent::Usage {
                    model_alias: spec.alias.clone(),
                    usage: response.usage,
                    cost_usd: record.estimated_usd,
                });
                Ok(response.turn)
            }
            Err(error) => {
                let mut ledger = lock_ledger(&self.ledger);
                // A cancelled call may already have reached the provider, and a streaming call
                // is billed for the tokens generated so far. The clients cannot cheaply tell
                // "cancelled before the request was sent" from "cancelled mid-flight", so every
                // cancellation is settled as unknown usage (conservative: the reservation keeps
                // counting against the cap) instead of being released.
                if matches!(error, ModelError::Cancelled) || error.billing_uncertain() {
                    // The provider may have billed the call: keep the reservation counting
                    // against the cap and record the call as unknown, never as free.
                    let _ = ledger.settle(
                        reservation,
                        Usage {
                            request_id: "unavailable".to_owned(),
                            input_tokens: None,
                            output_tokens: None,
                            cached_input_tokens: None,
                            cache_write_tokens: None,
                            reasoning_tokens: None,
                            latency_ms: 0,
                        },
                    );
                } else {
                    ledger.release(reservation);
                }
                drop(ledger);
                if matches!(error, ModelError::Cancelled) {
                    return Err(self.finish(events, StopReason::Cancelled));
                }
                Err(self.fail(events, format!("model call failed: {error}")))
            }
        }
    }

    /// Run tool calls one after another. Returns `true` when the turn was cancelled; results for
    /// calls that did not run are still appended so the history stays valid.
    async fn run_tool_calls(
        &mut self,
        calls: &[ToolCall],
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        let mut cancelled = false;
        for call in calls {
            if cancelled || cancel.is_cancelled() {
                cancelled = true;
                self.push_result(
                    call,
                    "Cancelled by the user before this call ran.".to_owned(),
                    true,
                    0,
                    events,
                );
                continue;
            }
            let started = Instant::now();
            let _ = events.send(AgentEvent::ToolCallStarted {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            self.record(&TranscriptEvent::ToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            let output = self.execute_call(call, events, cancel).await;
            if cancel.is_cancelled() {
                cancelled = true;
            }
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.push_result(call, output.text, output.is_error, duration_ms, events);
        }
        cancelled
    }

    fn push_result(
        &mut self,
        call: &ToolCall,
        output: String,
        is_error: bool,
        duration_ms: u64,
        events: &UnboundedSender<AgentEvent>,
    ) {
        let output = truncate_output(output, self.config.tool_result_max_chars);
        self.record(&TranscriptEvent::ToolResult {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output: output.clone(),
            is_error,
            duration_ms,
        });
        let _ = events.send(AgentEvent::ToolCallFinished {
            call_id: call.call_id.clone(),
            output: output.clone(),
            is_error,
            duration_ms,
        });
        self.history.push(Item::ToolResult {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output,
            is_error,
        });
    }

    async fn execute_call(
        &mut self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(spec) = self.tools.get(&call.name).cloned() else {
            return ToolOutput::error(format!("Unknown tool {:?}.", call.name));
        };
        let class = match &spec.source {
            ToolSource::Builtin(kind) => approval_class(*kind),
            ToolSource::Mcp { .. } => ApprovalClass::Mcp,
        };
        if let Value::String(raw) = &call.arguments {
            let shown: String = raw.chars().take(200).collect();
            return ToolOutput::error(format!("Tool arguments were not valid JSON: {shown}"));
        }
        if self.policy.requires_approval(class) {
            let (summary, diff) = match &spec.source {
                ToolSource::Builtin(kind) => match &self.builtins {
                    Some(builtins) => {
                        let preview = builtins.preview(*kind, &call.arguments);
                        (preview.summary, preview.diff)
                    }
                    None => (format!("{} {}", call.name, call.arguments), None),
                },
                ToolSource::Mcp { server, tool } => (
                    format!("Call {tool} on MCP server {server} with {}", call.arguments),
                    None,
                ),
            };
            let request = ApprovalRequest {
                id: self.next_approval_id,
                tool: call.name.clone(),
                class,
                summary,
                diff,
            };
            self.next_approval_id += 1;
            let (tx, rx) = oneshot::channel();
            let _ = events.send(AgentEvent::ApprovalRequested(request.clone(), tx));
            let decision = tokio::select! {
                () = cancel.cancelled() => Decision::Deny,
                answer = rx => answer.unwrap_or(Decision::Deny),
            };
            self.record(&TranscriptEvent::Approval {
                id: request.id,
                tool: request.tool.clone(),
                class,
                summary: request.summary.clone(),
                decision,
            });
            if !self.policy.record(class, decision) {
                return ToolOutput::error(DENIED_TEXT);
            }
        }
        match &spec.source {
            ToolSource::Builtin(kind) => match &self.builtins {
                Some(builtins) if self.config.builtins_enabled => {
                    builtins.run(*kind, &call.arguments, cancel).await
                }
                _ => ToolOutput::error("Built-in tools are disabled."),
            },
            ToolSource::Mcp { server, tool } => {
                let Some(connection) = self.mcp.get(server).cloned() else {
                    return ToolOutput::error(format!("MCP server {server} is not connected."));
                };
                tokio::select! {
                    () = cancel.cancelled() => ToolOutput::error("Cancelled by the user."),
                    outcome = connection.call_tool(tool, &call.arguments) => match outcome {
                        Ok(outcome) => ToolOutput { text: outcome.text, is_error: outcome.is_error },
                        Err(e) => ToolOutput::error(e.to_string()),
                    },
                }
            }
        }
    }
}
