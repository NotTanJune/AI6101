//! Bridge between the synchronous terminal loop and the asynchronous agent.
//!
//! [`AgentHandle`] starts a dedicated OS thread that runs a Tokio runtime. The runtime owns the
//! [`Agent`], the MCP connections and the session Tool KB. The terminal loop talks to it through
//! the [`AgentPort`] trait: it sends [`UiCommand`]s and drains [`UiEvent`]s once per tick.
//! [`ScriptedAgentHandle`] implements the same trait without a runtime, for tests.
//!
//! Approvals: the agent's `ApprovalRequested` event carries a oneshot sender. The bridge keeps
//! that sender in a map keyed by request id and forwards [`UiEvent::ApprovalNeeded`]; the UI
//! answers with [`UiCommand::Approve`]. A request that is never answered is denied when the turn
//! is cancelled or the bridge shuts down (the sender is dropped).
//!
//! Commands that change the agent (model, MCP, KB, drift) are only acted on while no turn is
//! running; during a turn the bridge accepts `Cancel`, `Approve` and `Shutdown` and reports
//! everything else as ignored. The UI applies the same rule before it sends.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use harness_agent::agent::{Agent, StopReason};
use harness_agent::approval::{ApprovalRequest, Decision};
use harness_agent::bridge::BlockingProvider;
use harness_agent::drift::{
    AdaptationOutcome, DriftReport, SessionKb, detect_drift, evaluate_candidate, kb_subset,
    live_kb, prepare_candidate,
};
use harness_agent::events::AgentEvent;
use harness_agent::live_canary::LiveCanaryExecutor;
use harness_agent::mcp::{
    DEFAULT_CALL_TIMEOUT, LiveTool, McpCanaryConfig, McpConfig, McpConnection, McpError,
};
use harness_agent::model::{ModelClient, TokenUsage};
use harness_agent::registry::{Effort, ModelRegistry, ProviderId};
use harness_agent::tools::ToolSource;
use harness_core::canary::PromotionDecision;
use harness_core::cost::CostLedger;
use harness_core::events::{AdaptationDiffEvent, AdaptationStatus, ErrorEvent, EventBody};
use harness_core::provider::MeteredProvider;
use harness_core::redact::Redactor;
use harness_core::{CanaryStatus, Strategy, ToolKb};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::artifacts::AdaptationJournal;

/// How long one MCP server may take to connect and list its tools.
pub const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Commands and events
// ---------------------------------------------------------------------------------------------

/// A request from the UI to the agent runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    /// Start a turn with this user message.
    SendUser(String),
    /// Cancel the running turn.
    Cancel,
    /// Switch model and effort.
    SetModel {
        /// Model alias.
        alias: String,
        /// Reasoning effort.
        effort: Effort,
    },
    /// Connect a configured MCP server.
    ConnectMcp(String),
    /// Disconnect an MCP server.
    DisconnectMcp(String),
    /// Snapshot an unmanaged server's live tools into the session KB.
    AdoptServer(String),
    /// Run an adaptation strategy and the canary for a drifted server.
    RunDrift {
        /// Server name.
        server: String,
        /// Strategy to run.
        strategy: Strategy,
    },
    /// Accept the candidate KB of a server (requires a passing canary).
    AcceptDrift {
        /// Server name.
        server: String,
    },
    /// Discard the candidate KB of a server.
    RejectDrift {
        /// Server name.
        server: String,
    },
    /// Answer an approval request.
    Approve {
        /// Request id from [`UiEvent::ApprovalNeeded`].
        id: u64,
        /// The user's decision.
        decision: Decision,
    },
    /// Stop the runtime.
    Shutdown,
}

/// An [`AgentEvent`] without the approval sender. Every other variant is passed through
/// unchanged, so no agent event is dropped on the way to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentUiEvent {
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
    /// A tool call finished.
    ToolCallFinished {
        /// Provider call id.
        call_id: String,
        /// Output text.
        output: String,
        /// Whether the call failed.
        is_error: bool,
        /// Wall-clock duration.
        duration_ms: u64,
    },
    /// Usage and cost of one model call.
    Usage {
        /// Model alias.
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

/// Result of [`split_agent_event`].
#[derive(Debug)]
pub enum AgentEventSplit {
    /// An event the UI can hold as plain data.
    Plain(AgentUiEvent),
    /// An approval request and the channel that answers it.
    Approval(ApprovalRequest, oneshot::Sender<Decision>),
}

/// Separate the approval sender from an agent event. The match is exhaustive on purpose: a new
/// [`AgentEvent`] variant fails to compile here until the UI side handles it.
pub fn split_agent_event(event: AgentEvent) -> AgentEventSplit {
    use AgentEventSplit::{Approval, Plain};
    match event {
        AgentEvent::TurnStarted => Plain(AgentUiEvent::TurnStarted),
        AgentEvent::TextDelta(text) => Plain(AgentUiEvent::TextDelta(text)),
        AgentEvent::ToolCallStarted {
            call_id,
            name,
            arguments,
        } => Plain(AgentUiEvent::ToolCallStarted {
            call_id,
            name,
            arguments,
        }),
        AgentEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
            duration_ms,
        } => Plain(AgentUiEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
            duration_ms,
        }),
        AgentEvent::ApprovalRequested(request, reply) => Approval(request, reply),
        AgentEvent::Usage {
            model_alias,
            usage,
            cost_usd,
        } => Plain(AgentUiEvent::Usage {
            model_alias,
            usage,
            cost_usd,
        }),
        AgentEvent::Drift(report) => Plain(AgentUiEvent::Drift(report)),
        AgentEvent::Notice(text) => Plain(AgentUiEvent::Notice(text)),
        AgentEvent::Error(text) => Plain(AgentUiEvent::Error(text)),
        AgentEvent::TurnFinished { reason } => Plain(AgentUiEvent::TurnFinished { reason }),
    }
}

/// Budget figures from the cost ledger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostSnapshot {
    /// USD spent on priced calls.
    pub spent_usd: f64,
    /// USD held for calls whose usage never came back (for example cancelled calls).
    pub unreconciled_usd: f64,
    /// Cap minus spent, unreconciled and in-flight amounts.
    pub remaining_usd: f64,
    /// Session cap.
    pub cap_usd: f64,
}

impl CostSnapshot {
    /// Read the figures from a ledger.
    pub fn from_ledger(ledger: &Mutex<CostLedger>) -> Self {
        let ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
        Self {
            spent_usd: ledger.spent_usd(),
            unreconciled_usd: ledger.unreconciled_usd(),
            remaining_usd: ledger.remaining_usd(),
            cap_usd: ledger.cap_usd(),
        }
    }
}

/// Connection state of an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpState {
    /// Configured but not connected.
    Disconnected,
    /// Connection in progress.
    Connecting,
    /// Connected.
    Connected,
    /// The last connection attempt failed.
    Failed(String),
}

/// A tool exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposedTool {
    /// Name the model sees.
    pub name: String,
    /// Description the model sees (from the KB, not the live server).
    pub description: String,
}

/// Status of one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    /// Server name.
    pub name: String,
    /// Connection state.
    pub state: McpState,
    /// Tools the live server lists.
    pub live_tools: usize,
    /// Whether the session KB has cards for this server. An unmanaged server exposes no tools.
    pub managed: bool,
    /// Tools currently exposed to the model.
    pub exposed: Vec<ExposedTool>,
}

/// Summary of the session Tool KB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbSummary {
    /// KB id.
    pub kb_id: String,
    /// KB version label.
    pub server_version: String,
    /// Card count per server.
    pub servers: Vec<(String, usize)>,
}

impl KbSummary {
    /// Summarise a KB.
    pub fn of(kb: &ToolKb) -> Self {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for card in &kb.cards {
            *counts.entry(card.server_id.clone()).or_default() += 1;
        }
        Self {
            kb_id: kb.kb_id.clone(),
            server_version: kb.server_version.clone(),
            servers: counts.into_iter().collect(),
        }
    }
}

/// One canary probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLine {
    /// Probe id.
    pub probe_id: Option<String>,
    /// Tool probed.
    pub tool_id: Option<String>,
    /// Whether this was the control probe.
    pub is_control: bool,
    /// Probe result.
    pub status: CanaryStatus,
    /// Expected observation.
    pub expected: Option<String>,
    /// Actual observation.
    pub actual: Option<String>,
}

/// Result of an adaptation strategy and the canary for one server.
#[derive(Debug, Clone, PartialEq)]
pub struct CanarySummary {
    /// Server name.
    pub server: String,
    /// Strategy that ran.
    pub strategy: Strategy,
    /// Aggregate canary status.
    pub status: CanaryStatus,
    /// One-line summary from the canary.
    pub summary: String,
    /// Probe results in execution order.
    pub probes: Vec<ProbeLine>,
    /// A control probe failed: the candidate damaged an unchanged tool.
    pub collateral_damage: bool,
    /// What core's promotion rule decided (`promote`, `blocked: ...`, `unchanged`).
    pub decision: String,
    /// Whether the candidate may be accepted (the canary passed and the rule says promote).
    pub can_accept: bool,
}

/// An event from the agent runtime to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// An agent event other than an approval request.
    Agent(AgentUiEvent),
    /// The agent waits for an approval. Answer with [`UiCommand::Approve`].
    ApprovalNeeded {
        /// Request id.
        id: u64,
        /// What the agent wants to do.
        request: ApprovalRequest,
    },
    /// The model or effort changed (also sent once at start).
    ModelChanged {
        /// Model alias.
        alias: String,
        /// Reasoning effort.
        effort: Effort,
    },
    /// Fresh budget figures.
    Cost(CostSnapshot),
    /// Status of an MCP server.
    McpStatus(McpServerStatus),
    /// The session KB changed (also sent once at start).
    KbSummary(KbSummary),
    /// Drift between the KB and a live server. An empty report clears the drift.
    DriftReady {
        /// Server name.
        server: String,
        /// The report.
        report: DriftReport,
    },
    /// A strategy and canary finished.
    CanaryReady(CanarySummary),
    /// A strategy or canary run failed.
    DriftFailed {
        /// Server name.
        server: String,
        /// Error text.
        message: String,
    },
    /// Informational message from the runtime.
    Notice(String),
    /// Error from the runtime.
    Error(String),
    /// The runtime cannot continue.
    Fatal(String),
}

// ---------------------------------------------------------------------------------------------
// AgentPort
// ---------------------------------------------------------------------------------------------

/// What the terminal loop needs from the agent side.
pub trait AgentPort {
    /// Send a command. Never blocks.
    fn send(&mut self, command: UiCommand);

    /// Take every event that arrived since the last call.
    fn poll(&mut self) -> Vec<UiEvent>;

    /// Ask the runtime to stop and wait up to `timeout` for it. Safe to call more than once.
    /// Returns whether the runtime stopped in time.
    fn shutdown(&mut self, timeout: Duration) -> bool;
}

// ---------------------------------------------------------------------------------------------
// MCP launcher
// ---------------------------------------------------------------------------------------------

/// Future returned by [`McpLauncher::connect`].
pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<McpConnection, McpError>> + Send + 'a>>;

/// Opens MCP connections by server name. Tests supply in-process servers.
pub trait McpLauncher: Send + Sync {
    /// Connect the server called `name`.
    fn connect<'a>(&'a self, name: &'a str) -> ConnectFuture<'a>;

    /// Explicit configuration of disposable state and its reset tool. Absent by default.
    fn canary_config(&self, _name: &str) -> Option<McpCanaryConfig> {
        None
    }

    /// Redactor including server credentials, for persisted adaptation evidence.
    fn artifact_redactor(&self) -> Redactor {
        Redactor::default()
    }
}

/// Launcher backed by an `mcp.json` configuration: stdio servers are spawned (stderr goes to
/// `<log_dir>/mcp-<name>.log`), URL servers use streamable HTTP.
#[derive(Debug, Clone)]
pub struct ConfigLauncher {
    config: McpConfig,
    log_dir: PathBuf,
}

impl ConfigLauncher {
    /// Launcher for `config`, writing server stderr logs under `log_dir`.
    pub fn new(config: McpConfig, log_dir: impl Into<PathBuf>) -> Self {
        Self {
            config,
            log_dir: log_dir.into(),
        }
    }

    /// Server names in the configuration.
    pub fn server_names(&self) -> Vec<String> {
        self.config.servers.keys().cloned().collect()
    }
}

impl McpLauncher for ConfigLauncher {
    fn canary_config(&self, name: &str) -> Option<McpCanaryConfig> {
        self.config.servers.get(name).and_then(|s| s.canary.clone())
    }

    fn artifact_redactor(&self) -> Redactor {
        let mut redactor = Redactor::new(
            self.config
                .servers
                .values()
                .flat_map(|s| s.env.values().cloned())
                .collect(),
        );
        fn add_arguments(redactor: &mut Redactor, value: &Value) {
            match value {
                Value::String(value) => redactor.add_secret(value),
                Value::Array(values) => values.iter().for_each(|v| add_arguments(redactor, v)),
                Value::Object(values) => values.values().for_each(|v| add_arguments(redactor, v)),
                _ => {}
            }
        }
        for server in self.config.servers.values() {
            if let Some(canary) = &server.canary {
                add_arguments(&mut redactor, &canary.reset_arguments);
            }
        }
        redactor
    }

    fn connect<'a>(&'a self, name: &'a str) -> ConnectFuture<'a> {
        Box::pin(async move {
            let server = self
                .config
                .servers
                .get(name)
                .ok_or_else(|| McpError::Config(format!("no MCP server named {name:?}")))?;
            if let Some(url) = &server.url {
                return McpConnection::connect_http(name, url, DEFAULT_CALL_TIMEOUT).await;
            }
            let safe: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let log = self.log_dir.join(format!("mcp-{safe}.log"));
            McpConnection::connect_stdio(name, server, &log, DEFAULT_CALL_TIMEOUT).await
        })
    }
}

/// Launcher with no servers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMcp;

impl McpLauncher for NoMcp {
    fn connect<'a>(&'a self, name: &'a str) -> ConnectFuture<'a> {
        Box::pin(async move { Err(McpError::Config(format!("no MCP server named {name:?}"))) })
    }
}

// ---------------------------------------------------------------------------------------------
// AgentHandle
// ---------------------------------------------------------------------------------------------

/// Everything the agent runtime needs. Built by the caller (the CLI or a test); the TUI crate
/// never reads credentials.
pub struct AgentSetup {
    /// The agent, already holding its tools, ledger, policy and transcript.
    pub agent: Agent,
    /// The same model registry the agent was built with (for drift adaptation).
    pub models: ModelRegistry,
    /// The same clients the agent was built with (for drift adaptation).
    pub clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
    /// The session Tool KB. A server with cards in it is "managed".
    pub session_kb: SessionKb,
    /// Configured MCP server names, connected at start.
    pub mcp_servers: Vec<String>,
    /// How to open connections.
    pub launcher: Arc<dyn McpLauncher>,
}

/// Failure to start the runtime thread.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// The OS thread could not be started.
    #[error("cannot start the agent thread: {0}")]
    Thread(#[source] std::io::Error),
}

/// Handle to the agent runtime thread.
pub struct AgentHandle {
    commands: Option<UnboundedSender<UiCommand>>,
    events: Receiver<UiEvent>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHandle")
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl AgentHandle {
    /// Start the runtime thread.
    pub fn spawn(setup: AgentSetup) -> Result<Self, BridgeError> {
        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = channel();
        let (done_tx, done_rx) = channel();
        let thread = std::thread::Builder::new()
            .name("agent-runtime".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        let panic_events = event_tx.clone();
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                runtime.block_on(run(setup, command_rx, event_tx));
                            }));
                        if outcome.is_err() {
                            let _ = panic_events
                                .send(UiEvent::Fatal("the agent runtime panicked".to_owned()));
                        }
                        // Blocking tasks (a drift run) cannot be cancelled; do not wait forever.
                        runtime.shutdown_timeout(Duration::from_secs(2));
                    }
                    Err(e) => {
                        let _ = event_tx.send(UiEvent::Fatal(format!(
                            "cannot start the agent runtime: {e}"
                        )));
                    }
                }
                let _ = done_tx.send(());
            })
            .map_err(BridgeError::Thread)?;
        Ok(Self {
            commands: Some(command_tx),
            events: event_rx,
            done: done_rx,
            thread: Some(thread),
        })
    }
}

impl AgentPort for AgentHandle {
    fn send(&mut self, command: UiCommand) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(command);
        }
    }

    fn poll(&mut self) -> Vec<UiEvent> {
        self.events.try_iter().collect()
    }

    fn shutdown(&mut self, timeout: Duration) -> bool {
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(UiCommand::Shutdown);
        }
        let Some(thread) = self.thread.take() else {
            return true;
        };
        match self.done.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                true
            }
            // The thread is detached: process exit ends it.
            Err(RecvTimeoutError::Timeout) => false,
        }
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(UiCommand::Shutdown);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Scripted port for tests
// ---------------------------------------------------------------------------------------------

type Responder = Box<dyn FnMut(&UiCommand) -> Vec<UiEvent> + Send>;

#[derive(Default)]
struct ScriptedState {
    sent: Vec<UiCommand>,
    queue: VecDeque<UiEvent>,
    shutdowns: usize,
    responder: Option<Responder>,
}

/// Clonable view of a [`ScriptedAgentHandle`], to push events and inspect sent commands while
/// the port is borrowed by the terminal loop.
#[derive(Clone, Default)]
pub struct ScriptedHandle {
    state: Arc<Mutex<ScriptedState>>,
}

impl std::fmt::Debug for ScriptedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedHandle").finish_non_exhaustive()
    }
}

impl ScriptedHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue an event for the next `poll`.
    pub fn push(&self, event: UiEvent) {
        self.lock().queue.push_back(event);
    }

    /// Commands sent so far.
    pub fn sent(&self) -> Vec<UiCommand> {
        self.lock().sent.clone()
    }

    /// Number of `shutdown` calls.
    pub fn shutdown_count(&self) -> usize {
        self.lock().shutdowns
    }

    /// React to every sent command with these events (queued for the next `poll`).
    pub fn set_responder(
        &self,
        responder: impl FnMut(&UiCommand) -> Vec<UiEvent> + Send + 'static,
    ) {
        self.lock().responder = Some(Box::new(responder));
    }
}

/// An [`AgentPort`] without a runtime.
#[derive(Debug, Default)]
pub struct ScriptedAgentHandle {
    handle: ScriptedHandle,
}

impl ScriptedAgentHandle {
    /// Port with an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clonable view of this port.
    pub fn handle(&self) -> ScriptedHandle {
        self.handle.clone()
    }
}

impl AgentPort for ScriptedAgentHandle {
    fn send(&mut self, command: UiCommand) {
        let mut state = self.handle.lock();
        let replies = match state.responder.as_mut() {
            Some(responder) => responder(&command),
            None => Vec::new(),
        };
        state.sent.push(command);
        state.queue.extend(replies);
    }

    fn poll(&mut self) -> Vec<UiEvent> {
        self.handle.lock().queue.drain(..).collect()
    }

    fn shutdown(&mut self, _timeout: Duration) -> bool {
        self.handle.lock().shutdowns += 1;
        true
    }
}

// ---------------------------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------------------------

struct ServerEntry {
    conn: Option<Arc<McpConnection>>,
    live: Vec<LiveTool>,
    state: McpState,
}

struct Bridge {
    agent: Agent,
    models: ModelRegistry,
    clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
    kb: SessionKb,
    launcher: Arc<dyn McpLauncher>,
    servers: BTreeMap<String, ServerEntry>,
    ledger: Arc<Mutex<CostLedger>>,
    events: Sender<UiEvent>,
    pending: HashMap<u64, oneshot::Sender<Decision>>,
    outcomes: HashMap<String, PendingAdaptation>,
}

struct PendingAdaptation {
    outcome: AdaptationOutcome,
    journal: AdaptationJournal,
}

async fn run(
    setup: AgentSetup,
    mut commands: UnboundedReceiver<UiCommand>,
    events: Sender<UiEvent>,
) {
    let ledger = Arc::clone(setup.agent.ledger());
    let servers = setup
        .mcp_servers
        .iter()
        .map(|name| {
            (
                name.clone(),
                ServerEntry {
                    conn: None,
                    live: Vec::new(),
                    state: McpState::Disconnected,
                },
            )
        })
        .collect();
    let mut bridge = Bridge {
        agent: setup.agent,
        models: setup.models,
        clients: setup.clients,
        kb: setup.session_kb,
        launcher: setup.launcher,
        servers,
        ledger,
        events,
        pending: HashMap::new(),
        outcomes: HashMap::new(),
    };
    bridge.announce();
    let names: Vec<String> = bridge.servers.keys().cloned().collect();
    for name in names {
        bridge.connect_server(&name).await;
    }
    while let Some(command) = commands.recv().await {
        let stop = match command {
            UiCommand::Shutdown => true,
            UiCommand::SendUser(text) => bridge.run_turn(text, &mut commands).await,
            other => bridge.handle_idle(other, &mut commands).await,
        };
        if stop {
            break;
        }
    }
    bridge.shutdown_servers().await;
}

/// Lock-free helper: forward one agent event to the UI.
fn forward(
    pending: &mut HashMap<u64, oneshot::Sender<Decision>>,
    events: &Sender<UiEvent>,
    ledger: &Mutex<CostLedger>,
    event: AgentEvent,
) {
    match split_agent_event(event) {
        AgentEventSplit::Approval(request, reply) => {
            pending.insert(request.id, reply);
            let _ = events.send(UiEvent::ApprovalNeeded {
                id: request.id,
                request,
            });
        }
        AgentEventSplit::Plain(event) => {
            let cost_changed = matches!(
                event,
                AgentUiEvent::Usage { .. } | AgentUiEvent::TurnFinished { .. }
            );
            let _ = events.send(UiEvent::Agent(event));
            if cost_changed {
                let _ = events.send(UiEvent::Cost(CostSnapshot::from_ledger(ledger)));
            }
        }
    }
}

impl Bridge {
    fn emit(&self, event: UiEvent) {
        let _ = self.events.send(event);
    }

    fn notice(&self, text: impl Into<String>) {
        self.emit(UiEvent::Notice(text.into()));
    }

    fn error(&self, text: impl Into<String>) {
        self.emit(UiEvent::Error(text.into()));
    }

    fn announce(&mut self) {
        let config = self.agent.config();
        self.emit(UiEvent::ModelChanged {
            alias: config.model_alias.clone(),
            effort: config.effort,
        });
        self.emit(UiEvent::Cost(CostSnapshot::from_ledger(&self.ledger)));
        self.emit(UiEvent::KbSummary(KbSummary::of(self.kb.kb())));
        for name in self.servers.keys() {
            self.emit_mcp_status(name);
        }
        self.agent.record_session_start();
    }

    fn is_managed(&self, server: &str) -> bool {
        self.kb.kb().cards.iter().any(|c| c.server_id == server)
    }

    fn emit_mcp_status(&self, name: &str) {
        let Some(entry) = self.servers.get(name) else {
            return;
        };
        let exposed = self
            .agent
            .tools()
            .specs()
            .iter()
            .filter(|spec| matches!(&spec.source, ToolSource::Mcp { server, .. } if server == name))
            .map(|spec| ExposedTool {
                name: spec.exposed_name.clone(),
                description: spec.description.clone(),
            })
            .collect();
        self.emit(UiEvent::McpStatus(McpServerStatus {
            name: name.to_owned(),
            state: entry.state.clone(),
            live_tools: entry.live.len(),
            managed: self.is_managed(name),
            exposed,
        }));
    }

    /// Replace the exposed tools of `server` from the session KB (nothing for an unmanaged
    /// server). The previous registry is restored when the new tools clash with existing names.
    fn register_tools(&mut self, server: &str) -> Result<(), String> {
        let live = self
            .servers
            .get(server)
            .map(|e| e.live.clone())
            .unwrap_or_default();
        let backup = self.agent.tools().clone();
        self.agent.tools_mut().remove_mcp_server(server);
        if self.is_managed(server)
            && let Err(e) = self
                .agent
                .tools_mut()
                .add_mcp_from_kb(server, self.kb.kb(), &live)
        {
            *self.agent.tools_mut() = backup;
            return Err(e.to_string());
        }
        Ok(())
    }

    fn check_drift(&mut self, server: &str) {
        let live = self
            .servers
            .get(server)
            .map(|e| e.live.clone())
            .unwrap_or_default();
        let (tx, _rx) = unbounded_channel();
        let report = self.agent.report_drift(server, self.kb.kb(), &live, &tx);
        self.emit(UiEvent::DriftReady {
            server: server.to_owned(),
            report,
        });
    }

    async fn connect_server(&mut self, name: &str) {
        let Some(entry) = self.servers.get_mut(name) else {
            self.error(format!("no MCP server named {name:?} is configured"));
            return;
        };
        if entry.state == McpState::Connected {
            self.notice(format!("MCP server {name} is already connected"));
            return;
        }
        entry.state = McpState::Connecting;
        self.emit_mcp_status(name);

        let launcher = Arc::clone(&self.launcher);
        let attempt = tokio::time::timeout(MCP_CONNECT_TIMEOUT, async {
            let conn = launcher.connect(name).await?;
            match conn.list_tools().await {
                Ok(live) => Ok((conn, live)),
                Err(e) => {
                    conn.shutdown().await;
                    Err(e)
                }
            }
        })
        .await;
        let failure = match attempt {
            Ok(Ok((conn, live))) => {
                let conn = Arc::new(conn);
                self.agent.add_mcp_connection(name, Arc::clone(&conn));
                if let Some(entry) = self.servers.get_mut(name) {
                    entry.conn = Some(conn);
                    entry.live = live;
                    entry.state = McpState::Connected;
                }
                match self.register_tools(name) {
                    Ok(()) => None,
                    Err(e) => Some(format!("cannot expose the tools of {name}: {e}")),
                }
            }
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some(format!(
                "timed out after {}s",
                MCP_CONNECT_TIMEOUT.as_secs()
            )),
        };
        if let Some(message) = failure {
            if let Some(entry) = self.servers.get_mut(name)
                && entry.state != McpState::Connected
            {
                entry.state = McpState::Failed(message.clone());
            }
            self.error(format!("MCP server {name}: {message}"));
        } else if self.is_managed(name) {
            self.check_drift(name);
        }
        self.emit_mcp_status(name);
    }

    async fn disconnect_server(&mut self, name: &str) {
        let Some(entry) = self.servers.get_mut(name) else {
            self.error(format!("no MCP server named {name:?} is configured"));
            return;
        };
        let Some(conn) = entry.conn.take() else {
            self.notice(format!("MCP server {name} is not connected"));
            return;
        };
        entry.live.clear();
        entry.state = McpState::Disconnected;
        conn.shutdown().await;
        self.agent.tools_mut().remove_mcp_server(name);
        self.reject_drift_if_present(name, "discarded because the server disconnected");
        self.emit_mcp_status(name);
        self.notice(format!("disconnected MCP server {name}"));
    }

    async fn adopt(&mut self, server: &str) {
        let Some(conn) = self.servers.get(server).and_then(|e| e.conn.clone()) else {
            self.error(format!("MCP server {server} is not connected"));
            return;
        };
        if self.is_managed(server) {
            self.error(format!("server {server} is already managed by the KB"));
            return;
        }
        let live = match conn.list_tools().await {
            Ok(live) => live,
            Err(e) => {
                self.error(format!("cannot list the tools of {server}: {e}"));
                return;
            }
        };
        let count = live.len();
        let snapshot = live_kb(server, self.kb.kb(), &live);
        let mut merged = self.kb.kb().clone();
        merged.cards.retain(|c| c.server_id != server);
        merged.cards.extend(snapshot.cards);
        let previous = std::mem::replace(&mut self.kb, SessionKb::new(merged));
        if let Some(entry) = self.servers.get_mut(server) {
            entry.live = live;
        }
        if let Err(e) = self.register_tools(server) {
            self.kb = previous;
            self.error(format!("cannot adopt {server}: {e}"));
            return;
        }
        self.emit_mcp_status(server);
        self.emit(UiEvent::KbSummary(KbSummary::of(self.kb.kb())));
        self.notice(format!(
            "adopted {count} live tool(s) of {server} into the session KB"
        ));
    }

    fn set_model(&mut self, alias: &str, effort: Effort) {
        if let Ok(spec) = self.models.get(alias)
            && !self.clients.contains_key(&spec.provider)
        {
            self.error(format!(
                "cannot switch to {alias}: no {} client is available (the environment variable \
                 {} was not set at start-up)",
                spec.provider.as_str(),
                spec.api_key_env
            ));
            return;
        }
        match self.agent.set_model(alias, effort) {
            Ok(()) => self.emit(UiEvent::ModelChanged {
                alias: alias.to_owned(),
                effort,
            }),
            Err(e) => self.error(e.to_string()),
        }
    }

    async fn run_drift(
        &mut self,
        server: &str,
        strategy: Strategy,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        // Invalidate the previous candidate even when this attempt fails during preflight.
        self.reject_drift_if_present(server, "superseded by a new adaptation");
        let fail = |this: &Self, message: String| {
            this.emit(UiEvent::DriftFailed {
                server: server.to_owned(),
                message,
            });
        };
        let Some(conn) = self.servers.get(server).and_then(|e| e.conn.clone()) else {
            fail(self, format!("MCP server {server} is not connected"));
            return false;
        };
        if !self.is_managed(server) {
            fail(
                self,
                format!("server {server} is not managed by the KB; adopt it first"),
            );
            return false;
        }
        let live = match conn.list_tools().await {
            Ok(live) => live,
            Err(e) => {
                fail(self, format!("cannot list the tools of {server}: {e}"));
                return false;
            }
        };
        if let Some(entry) = self.servers.get_mut(server) {
            entry.live.clone_from(&live);
        }
        let (report, live_kb) = detect_drift(server, self.kb.kb(), &live);
        let subset = kb_subset(self.kb.kb(), server);
        let config = self.agent.config();
        let (alias, effort) = (config.model_alias.clone(), config.effort);
        let Ok(spec) = self.models.get(&alias).cloned() else {
            fail(self, format!("unknown model alias {alias:?}"));
            return false;
        };
        let Some(client) = self.clients.get(&spec.provider).cloned() else {
            fail(
                self,
                format!("no client configured for {}", spec.provider.as_str()),
            );
            return false;
        };
        let ledger = Arc::clone(&self.ledger);
        let handle = tokio::runtime::Handle::current();
        let diff = report.diff;
        let Some(session_dir) = self.agent.transcript_path().and_then(|p| p.parent()) else {
            fail(
                self,
                "adaptation requires a session transcript directory for evidence".to_owned(),
            );
            return false;
        };
        let mut redactor = self.launcher.artifact_redactor();
        for name in self.models.api_key_envs() {
            if let Ok(secret) = std::env::var(name) {
                redactor.add_secret(secret);
            }
        }
        let mut journal = match AdaptationJournal::create(
            session_dir,
            strategy,
            &subset,
            &live_kb,
            redactor.clone(),
        ) {
            Ok(journal) => journal,
            Err(e) => {
                fail(self, format!("cannot open adaptation evidence: {e}"));
                return false;
            }
        };
        let artifact_path = journal.dir().display().to_string();
        self.agent
            .record_notice(format!("adaptation evidence: {artifact_path}"));
        self.notice(format!("adaptation evidence: {artifact_path}"));
        let canary_config = self.launcher.canary_config(server);
        let server_name = server.to_owned();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        // Strategies are blocking and call the model through `block_on`: keep them off the
        // runtime workers.
        let mut work = tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<AdaptationOutcome, String> {
                journal.metadata(serde_json::json!({
                    "server": server_name, "strategy": strategy,
                    "model_alias": alias, "model": spec.model_id, "effort": effort,
                    "canary_execution": "live_mcp", "task_completion_verified": false,
                }))?;
                journal
                    .emitter
                    .emit(
                        Some(strategy),
                        EventBody::AdaptationDiff(AdaptationDiffEvent {
                            source_version: diff.source_version.clone(),
                            target_version: diff.target_version.clone(),
                            changes: diff.changes.clone(),
                            candidate_hash: None,
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                let mut provider = BlockingProvider::new(client, spec, effort, handle.clone())
                    .with_cancellation(worker_cancellation.clone());
                let candidate = {
                    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
                    let mut metered = MeteredProvider::new(
                        &mut provider,
                        &mut ledger,
                        &mut journal.emitter,
                        Some(strategy),
                    );
                    prepare_candidate(strategy, &subset, &live_kb, &diff, &mut metered)
                        .map_err(|e| e.to_string())?
                };
                journal.write_kb("candidate.json", &candidate.kb)?;
                let mut executor = LiveCanaryExecutor::new(
                    &server_name,
                    conn,
                    &live_kb,
                    canary_config.as_ref(),
                    handle,
                )
                .with_cancellation(worker_cancellation.clone());
                let outcome = evaluate_candidate(
                    strategy,
                    &subset,
                    &live_kb,
                    candidate,
                    &mut executor,
                    &mut journal.emitter,
                )
                .map_err(|e| e.to_string())?;
                let state = match &outcome.decision {
                    PromotionDecision::Promote(_) => "awaiting_acceptance",
                    PromotionDecision::Blocked { .. } => "blocked",
                    PromotionDecision::Unchanged { .. } => "unchanged",
                };
                journal.state(
                    state,
                    Some(&outcome.candidate.kb),
                    Some(&outcome.canary.summary()),
                )?;
                if !matches!(outcome.decision, PromotionDecision::Promote(_)) {
                    journal.outcome(
                        outcome.decision.adaptation_status(),
                        &outcome.canary.summary(),
                        &outcome.candidate.kb,
                        diff.affected_lineage_ids.iter().cloned().collect(),
                    )?;
                }
                Ok(outcome)
            })();
            match result {
                Ok(outcome) => Ok(PendingAdaptation { outcome, journal }),
                Err(message) => {
                    let message = redactor.redact_str(&message);
                    let event = journal.emitter.emit(
                        Some(strategy),
                        EventBody::Error(ErrorEvent {
                            message: message.clone(),
                            fatal: true,
                        }),
                    );
                    let state = journal.state(
                        if worker_cancellation.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        },
                        None,
                        Some(&message),
                    );
                    match (event, state) {
                        (Ok(_), Ok(())) => Err(message),
                        _ => Err(format!(
                            "{message}; could not finish writing adaptation evidence"
                        )),
                    }
                }
            }
        });
        let mut stop = false;
        let joined = loop {
            tokio::select! {
                joined = &mut work => break joined,
                command = commands.recv() => match command {
                    Some(UiCommand::Shutdown) | None => {
                        cancellation.cancel();
                        stop = true;
                        break work.await;
                    }
                    Some(UiCommand::Cancel) => cancellation.cancel(),
                    Some(UiCommand::Approve { .. }) => {}
                    Some(other) => self.notice(format!(
                        "ignored while a drift run is in progress: {}",
                        describe(&other)
                    )),
                },
            }
        };
        self.emit(UiEvent::Cost(CostSnapshot::from_ledger(&self.ledger)));
        match joined {
            Ok(Ok(mut pending)) if cancellation.is_cancelled() => {
                let reason = "adaptation cancelled; prior KB retained";
                let saved = pending
                    .journal
                    .state(
                        "cancelled",
                        Some(&pending.outcome.candidate.kb),
                        Some(reason),
                    )
                    .and_then(|()| {
                        pending.journal.outcome(
                            AdaptationStatus::Unchanged,
                            reason,
                            &pending.outcome.candidate.kb,
                            pending.outcome.candidate.changed_lineage_ids.clone(),
                        )
                    });
                if let Err(e) = saved {
                    fail(self, format!("{reason}; evidence write failed: {e}"));
                } else {
                    fail(self, reason.to_owned());
                }
            }
            Ok(Ok(pending)) => {
                let mut summary = summarize(server, strategy, &pending.outcome);
                summary.summary = pending.journal.redact(&summary.summary);
                summary.decision = pending.journal.redact(&summary.decision);
                for probe in &mut summary.probes {
                    probe.expected = probe.expected.as_deref().map(|s| pending.journal.redact(s));
                    probe.actual = probe.actual.as_deref().map(|s| pending.journal.redact(s));
                }
                self.outcomes.insert(server.to_owned(), pending);
                self.emit(UiEvent::CanaryReady(summary));
            }
            Ok(Err(e)) => fail(self, e.to_string()),
            Err(e) => fail(self, format!("the strategy task failed: {e}")),
        }
        stop
    }

    fn accept_drift(&mut self, server: &str) {
        let Some(pending) = self.outcomes.get(server) else {
            self.error(format!(
                "no candidate KB for {server}; run a strategy first"
            ));
            return;
        };
        if !matches!(pending.outcome.decision, PromotionDecision::Promote(_)) {
            self.error(format!(
                "the canary did not pass for {server}; the candidate cannot be accepted"
            ));
            return;
        }
        let Some(mut pending) = self.outcomes.remove(server) else {
            return;
        };
        let previous = self.kb.clone();
        let changed = self.kb.accept(&pending.outcome.candidate);
        if let Err(e) = self.register_tools(server) {
            self.kb = previous;
            let _ = pending
                .journal
                .state("failed", Some(&pending.outcome.candidate.kb), Some(&e));
            let _ = pending.journal.outcome(
                AdaptationStatus::Blocked,
                &e,
                &pending.outcome.candidate.kb,
                pending.outcome.candidate.changed_lineage_ids.clone(),
            );
            self.error(format!("cannot expose the accepted KB of {server}: {e}"));
            return;
        }
        let mut export_written = false;
        let saved = (|| {
            pending.journal.write_kb("accepted.json", self.kb.kb())?;
            export_written = true;
            pending
                .journal
                .state("accepted", Some(&pending.outcome.candidate.kb), None)?;
            pending.journal.outcome(
                AdaptationStatus::Promoted,
                "explicitly accepted in the TUI",
                &pending.outcome.candidate.kb,
                pending.outcome.candidate.changed_lineage_ids.clone(),
            )
        })();
        if let Err(e) = saved {
            self.kb = previous;
            let restore = self.register_tools(server);
            if export_written {
                let _ = std::fs::remove_file(pending.journal.dir().join("accepted.json"));
            }
            let _ = pending
                .journal
                .state("failed", Some(&pending.outcome.candidate.kb), Some(&e));
            self.error(format!(
                "KB acceptance blocked because evidence could not be saved: {e}"
            ));
            if let Err(restore_error) = restore {
                self.error(format!(
                    "could not restore prior tool registry: {restore_error}"
                ));
            }
            return;
        }
        self.agent.record_notice(format!(
            "accepted KB exported to {}",
            pending.journal.dir().join("accepted.json").display(),
        ));
        self.emit_mcp_status(server);
        self.emit(UiEvent::KbSummary(KbSummary::of(self.kb.kb())));
        self.check_drift(server);
        self.notice(format!(
            "accepted the candidate KB for {server}: {changed} card(s) changed"
        ));
    }

    fn reject_drift(&mut self, server: &str) {
        if self.reject_drift_if_present(server, "rejected by the user") {
            self.notice(format!(
                "rejected the candidate KB for {server}; KB unchanged"
            ));
        } else {
            self.notice(format!("no candidate KB for {server}"));
        }
    }

    fn reject_drift_if_present(&mut self, server: &str, reason: &str) -> bool {
        let Some(mut pending) = self.outcomes.remove(server) else {
            return false;
        };
        let recorded = pending
            .journal
            .outcome(
                AdaptationStatus::Unchanged,
                reason,
                &pending.outcome.candidate.kb,
                pending.outcome.candidate.changed_lineage_ids.clone(),
            )
            .and_then(|()| {
                pending.journal.state(
                    "rejected",
                    Some(&pending.outcome.candidate.kb),
                    Some(reason),
                )
            });
        if let Err(e) = recorded {
            self.error(format!("could not record rejected candidate: {e}"));
        }
        self.agent
            .record_notice(format!("candidate for {server}: {reason}"));
        true
    }

    async fn handle_idle(
        &mut self,
        command: UiCommand,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        match command {
            UiCommand::SetModel { alias, effort } => self.set_model(&alias, effort),
            UiCommand::ConnectMcp(name) => self.connect_server(&name).await,
            UiCommand::DisconnectMcp(name) => self.disconnect_server(&name).await,
            UiCommand::AdoptServer(server) => self.adopt(&server).await,
            UiCommand::RunDrift { server, strategy } => {
                return self.run_drift(&server, strategy, commands).await;
            }
            UiCommand::AcceptDrift { server } => self.accept_drift(&server),
            UiCommand::RejectDrift { server } => self.reject_drift(&server),
            // Nothing to cancel or answer while idle; a late answer is harmless.
            UiCommand::Cancel | UiCommand::Approve { .. } => {}
            UiCommand::SendUser(_) | UiCommand::Shutdown => {}
        }
        false
    }

    /// Run one turn, forwarding events and serving `Cancel`, `Approve` and `Shutdown` while it
    /// runs. Returns whether the runtime should stop.
    async fn run_turn(
        &mut self,
        text: String,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        let Self {
            agent,
            pending,
            events,
            ledger,
            ..
        } = self;
        let (tx, mut rx) = unbounded_channel::<AgentEvent>();
        let cancel = CancellationToken::new();
        let mut stop = false;
        let mut commands_open = true;
        {
            let turn = agent.run_turn(&text, tx, cancel.clone());
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    Some(event) = rx.recv() => forward(pending, events, ledger, event),
                    command = commands.recv(), if commands_open => match command {
                        Some(UiCommand::Cancel) => {
                            cancel.cancel();
                            pending.clear();
                        }
                        Some(UiCommand::Approve { id, decision }) => {
                            if let Some(reply) = pending.remove(&id) {
                                let _ = reply.send(decision);
                            }
                        }
                        Some(UiCommand::Shutdown) => {
                            stop = true;
                            cancel.cancel();
                            pending.clear();
                        }
                        None => {
                            commands_open = false;
                            stop = true;
                            cancel.cancel();
                            pending.clear();
                        }
                        Some(other) => {
                            let _ = events.send(UiEvent::Notice(format!(
                                "ignored while a turn is running: {}",
                                describe(&other)
                            )));
                        }
                    },
                    _ = &mut turn => break,
                }
            }
        }
        while let Ok(event) = rx.try_recv() {
            forward(pending, events, ledger, event);
        }
        pending.clear();
        stop
    }

    async fn shutdown_servers(&mut self) {
        let pending: Vec<String> = self.outcomes.keys().cloned().collect();
        for server in pending {
            self.reject_drift_if_present(&server, "discarded because the session ended");
        }
        let conns: Vec<Arc<McpConnection>> = self
            .servers
            .values_mut()
            .filter_map(|entry| entry.conn.take())
            .collect();
        for conn in conns {
            conn.shutdown().await;
        }
    }
}

fn describe(command: &UiCommand) -> &'static str {
    match command {
        UiCommand::SendUser(_) => "message",
        UiCommand::Cancel => "cancel",
        UiCommand::SetModel { .. } => "model change",
        UiCommand::ConnectMcp(_) => "MCP connect",
        UiCommand::DisconnectMcp(_) => "MCP disconnect",
        UiCommand::AdoptServer(_) => "KB adopt",
        UiCommand::RunDrift { .. } => "drift run",
        UiCommand::AcceptDrift { .. } => "drift accept",
        UiCommand::RejectDrift { .. } => "drift reject",
        UiCommand::Approve { .. } => "approval",
        UiCommand::Shutdown => "shutdown",
    }
}

fn summarize(server: &str, strategy: Strategy, outcome: &AdaptationOutcome) -> CanarySummary {
    let (can_accept, decision) = match &outcome.decision {
        PromotionDecision::Promote(_) => (true, "promote".to_owned()),
        PromotionDecision::Blocked { reason, .. } => (false, format!("blocked: {reason}")),
        PromotionDecision::Unchanged { .. } => (false, "unchanged".to_owned()),
    };
    CanarySummary {
        server: server.to_owned(),
        strategy,
        status: outcome.canary.status,
        summary: outcome.canary.summary(),
        probes: outcome
            .canary
            .results
            .iter()
            .map(|r| ProbeLine {
                probe_id: r.probe_id.clone(),
                tool_id: r.tool_id.clone(),
                is_control: r.is_control,
                status: r.status,
                expected: r.expected.clone(),
                actual: r.actual.clone(),
            })
            .collect(),
        collateral_damage: outcome.canary.collateral_damage,
        decision,
        can_accept,
    }
}
