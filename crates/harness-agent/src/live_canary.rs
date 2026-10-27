//! Live MCP canary execution against explicitly isolated, resettable server state.

use std::collections::BTreeSet;
use std::sync::Arc;

use harness_core::canary::{Probe, ProbeError, ProbeExecutor, ProbeObservation};
use harness_core::model::ToolKb;
use serde_json::Value;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::mcp::{CallOutcome, McpCanaryConfig, McpConnection};

/// Synchronous canary adapter for one connected MCP server.
///
/// Construct and run the adapter inside `tokio::task::spawn_blocking`, passing the
/// live runtime's handle. Each probe requires a successful real reset call first.
/// Missing isolation permission, an absent reset tool, or a server identity mismatch
/// disables all calls. A passing probe proves only that MCP accepted its execution;
/// it does not prove task completion or validate the returned business result.
pub struct LiveCanaryExecutor {
    connection: Arc<McpConnection>,
    runtime: Handle,
    config: Option<McpCanaryConfig>,
    tools: BTreeSet<(String, String)>,
    isolated: bool,
    reset_ready: bool,
    cancellation: CancellationToken,
}

impl LiveCanaryExecutor {
    /// Bind live target tool identities to one server and its isolation contract.
    ///
    /// `target` must be the live KB from that same connection. The configured reset
    /// tool must appear in its listing, and reset arguments must be a JSON object.
    pub fn new(
        server: &str,
        connection: Arc<McpConnection>,
        target: &ToolKb,
        config: Option<&McpCanaryConfig>,
        runtime: Handle,
    ) -> Self {
        let isolated = connection.name() == server
            && target.cards.iter().all(|card| card.server_id == server)
            && config.is_some_and(|config| {
                config.isolated
                    && !config.reset_tool.trim().is_empty()
                    && config.reset_arguments.is_object()
                    && target
                        .cards
                        .iter()
                        .any(|card| card.tool_id == config.reset_tool)
            });
        Self {
            connection,
            runtime,
            config: config.cloned(),
            tools: target
                .cards
                .iter()
                .map(|card| (card.lineage_id.clone(), card.tool_id.clone()))
                .collect(),
            isolated,
            reset_ready: false,
            cancellation: CancellationToken::new(),
        }
    }

    /// Stop waiting for MCP calls when the owning session is cancelled.
    ///
    /// Cancellation permanently disables later reset/probe calls. Requests already
    /// sent to the server may still run; cancellation does not roll them back.
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.isolated &= !self.cancellation.is_cancelled() && !token.is_cancelled();
        self.cancellation = token;
        self
    }

    fn call(&self, tool: &str, arguments: &Value) -> Result<CallOutcome, String> {
        self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(
                    "live canary cancelled; already sent MCP calls may continue on the server".into()
                ),
                outcome = self.connection.call_tool(tool, arguments) => {
                    outcome.map_err(|error| error.to_string())
                }
            }
        })
    }
}

impl ProbeExecutor for LiveCanaryExecutor {
    fn is_isolated(&self) -> bool {
        self.isolated && !self.cancellation.is_cancelled()
    }

    fn reset(&mut self) -> Result<(), ProbeError> {
        self.reset_ready = false;
        if self.cancellation.is_cancelled() {
            self.isolated = false;
            return Err(ProbeError::Reset("live canary cancelled".into()));
        }
        if !self.isolated {
            return Err(ProbeError::Reset(
                "live server lacks an isolated reset contract or matching identity".into(),
            ));
        }
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| ProbeError::Reset("live server has no canary configuration".into()))?;
        let outcome = match self.call(&config.reset_tool, &config.reset_arguments) {
            Ok(outcome) => outcome,
            Err(error) => {
                // A timed-out request can still be running on the server. Do not start
                // another reset or probe against a baseline whose state is unknown.
                self.isolated = false;
                return Err(ProbeError::Reset(error));
            }
        };
        if outcome.is_error {
            self.isolated = false;
            return Err(ProbeError::Reset(format!(
                "MCP reset tool reported an error: {}",
                outcome.text
            )));
        }
        self.reset_ready = true;
        Ok(())
    }

    fn execute(&mut self, probe: &Probe) -> Result<ProbeObservation, ProbeError> {
        let reset_ready = std::mem::take(&mut self.reset_ready);
        if self.cancellation.is_cancelled() {
            self.isolated = false;
            return Err(ProbeError::Execution("live canary cancelled".into()));
        }
        if !self.isolated || !reset_ready {
            return Err(ProbeError::Execution(
                "live probe requires isolation permission and a successful fresh reset".into(),
            ));
        }
        if !self
            .tools
            .contains(&(probe.lineage_id.clone(), probe.tool_id.clone()))
        {
            return Err(ProbeError::Execution(
                "probe tool and lineage do not belong to this live server".into(),
            ));
        }
        let outcome = match self.call(&probe.tool_id, &Value::Object(probe.arguments.clone())) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.isolated = false;
                return Err(ProbeError::Execution(error));
            }
        };
        if outcome.is_error {
            Ok(ProbeObservation::fail(format!(
                "MCP tool reported an error: {}",
                outcome.text
            )))
        } else {
            Ok(ProbeObservation::pass(format!(
                "MCP call accepted; task completion not verified: {}",
                outcome.text
            )))
        }
    }
}
