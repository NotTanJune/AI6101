//! Run-event types (`run-event/1`), sinks, the [`EventEmitter`] and JSONL reading.
//!
//! Wire format: one JSON object per line. Rust writes objects with sorted keys, so output is
//! byte-for-byte deterministic for a given event sequence and clock.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::clock::Clock;
use crate::config::Strategy;
use crate::model::ChangeClass;
use crate::redact::Redactor;
use crate::schema;

/// Schema version string carried by every run event.
pub const RUN_EVENT_SCHEMA_VERSION: &str = "run-event/1";

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Failure while reading, validating, emitting or writing run events.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// Filesystem failure.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path involved (empty when unknown).
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The line is not valid JSON.
    #[error("line {line}: invalid JSON: {message}")]
    BadJson {
        /// 1-based line number.
        line: usize,
        /// Parser message.
        message: String,
    },
    /// The line is blank.
    #[error("line {line}: blank line")]
    BlankLine {
        /// 1-based line number.
        line: usize,
    },
    /// The event declares a schema version this build does not support (fails closed).
    #[error(
        "line {line}: unsupported schema_version {found} (supported: \"{RUN_EVENT_SCHEMA_VERSION}\")"
    )]
    UnsupportedSchemaVersion {
        /// 1-based line number.
        line: usize,
        /// The offending value as written.
        found: String,
    },
    /// The event does not match the schema.
    #[error("line {line}: schema validation failed: {}", errors.join("; "))]
    Schema {
        /// 1-based line number.
        line: usize,
        /// Readable validation errors.
        errors: Vec<String>,
    },
    /// The event passed the schema but could not be converted to the typed model.
    #[error("line {line}: cannot decode event: {message}")]
    Decode {
        /// 1-based line number.
        line: usize,
        /// Decoder message.
        message: String,
    },
    /// An in-memory event failed validation (no source line).
    #[error("event failed validation: {}", errors.join("; "))]
    Invalid {
        /// Readable validation errors.
        errors: Vec<String>,
    },
    /// The event could not be serialized.
    #[error("cannot serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A sink rejected the event.
    #[error("sink error: {0}")]
    Sink(String),
}

impl EventError {
    /// 1-based line number the error refers to, when it has one.
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::BadJson { line, .. }
            | Self::BlankLine { line }
            | Self::UnsupportedSchemaVersion { line, .. }
            | Self::Schema { line, .. }
            | Self::Decode { line, .. } => Some(*line),
            Self::Io { .. } | Self::Invalid { .. } | Self::Serialize(_) | Self::Sink(_) => None,
        }
    }

    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------------------------

/// Which workstream wrote the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// Written by the Rust harness.
    Rust,
    /// Written by the Python benchmark runner.
    Python,
}

/// Kind of change recorded in an adaptation diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Present only in the target version.
    Added,
    /// Present only in the source version.
    Removed,
    /// Same lineage, different name or id.
    Renamed,
    /// Same tool, changed content.
    Modified,
}

/// Result of the adaptation step for one strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptationStatus {
    /// Candidate accepted as the new KB.
    Promoted,
    /// Candidate blocked (for example by a failed canary); prior KB kept.
    Blocked,
    /// No candidate; KB unchanged.
    Unchanged,
    /// Candidate rejected by validation.
    Rejected,
}

/// Phase of the pipeline a model call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Tool KB adaptation.
    Adaptation,
    /// Canary probing.
    Canary,
    /// Completion verification.
    Verifier,
    /// Probe generation.
    ProbeGeneration,
    /// Agent execution.
    Agent,
    /// Anything else.
    Other,
}

/// How a model call was priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostStatus {
    /// Priced from returned usage and the rate card.
    Priced,
    /// Usage or price unknown; never zero.
    Unknown,
    /// Mock provider; no real spend.
    Mock,
    /// Local inference; tracked by latency, not USD.
    Local,
}

/// Canary gate outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryStatus {
    /// Probe behaved as expected.
    Pass,
    /// Probe failed.
    Fail,
    /// Probe could not decide.
    Inconclusive,
    /// No candidate to probe (no-update runs).
    NotApplicable,
}

/// Completion-verifier outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifierOutcome {
    /// Task confirmed complete.
    ConfirmedSuccess,
    /// Task confirmed not complete.
    ConfirmedFailure,
    /// Not enough evidence.
    Inconclusive,
}

/// Metric id. Treated as opaque identifiers owned by the Python workstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Metric `tf`.
    Tf,
    /// Metric `pe`.
    Pe,
    /// Metric `ecs`.
    Ecs,
    /// Recovery metric.
    Recovery,
    /// Retention metric.
    Retention,
}

/// Failure category of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Stale knowledge was not updated.
    UnderUpdate,
    /// Knowledge was changed that should have stayed.
    OverUpdate,
}

/// Final status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Completed normally.
    Ok,
    /// Failed.
    Failed,
    /// Cancelled by the operator.
    Cancelled,
    /// Stopped because the budget cap was reached.
    BudgetExhausted,
}

// ---------------------------------------------------------------------------------------------
// Event bodies
// ---------------------------------------------------------------------------------------------

/// `config` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigEvent {
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Judge version.
    pub judge_version: String,
    /// Experiment seed.
    pub seed: u64,
    /// Repetition number.
    pub repetition: u32,
    /// Local dispatch cap in USD.
    pub budget_cap_usd: Option<f64>,
    /// Fixed task ids.
    pub task_ids: Vec<String>,
    /// Source server version label.
    pub source_version: String,
    /// Target server version label.
    pub target_version: String,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling value.
    pub top_p: Option<f64>,
    /// Output token limit.
    pub max_output_tokens: Option<u32>,
}

/// One change between two versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// Stable lineage id of the tool.
    pub lineage_id: String,
    /// Source-native tool id.
    pub tool_id: String,
    /// Level of the change.
    pub class: ChangeClass,
    /// Kind of the change.
    pub kind: ChangeKind,
    /// Changed field path, when field-level.
    pub field_path: Option<String>,
    /// Hash before the change.
    pub old_hash: Option<String>,
    /// Hash after the change.
    pub new_hash: Option<String>,
}

/// `adaptation_diff` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptationDiffEvent {
    /// Source server version label.
    pub source_version: String,
    /// Target server version label.
    pub target_version: String,
    /// Detected changes.
    pub changes: Vec<ChangeRecord>,
    /// Hash of the candidate KB, when one exists.
    pub candidate_hash: Option<String>,
}

/// `adaptation_outcome` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptationOutcomeEvent {
    /// Outcome status.
    pub status: AdaptationStatus,
    /// Human-readable reason.
    pub reason: String,
    /// Hash of the candidate KB, when one exists.
    pub candidate_hash: Option<String>,
    /// Hash of the prior KB (kept on rollback).
    pub prior_hash: String,
    /// Lineage ids that changed.
    pub changed_lineage_ids: Vec<String>,
}

/// `model_call` body. Also the cost record shared with the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCallEvent {
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Pipeline phase.
    pub phase: Phase,
    /// Provider request id.
    pub request_id: String,
    /// Input tokens, when returned.
    pub input_tokens: Option<u64>,
    /// Output tokens, when returned.
    pub output_tokens: Option<u64>,
    /// Wall-clock latency of the call.
    pub latency_ms: u64,
    /// Rate card id used for pricing.
    pub rate_card_id: Option<String>,
    /// Rate card date.
    pub rate_card_date: Option<String>,
    /// Estimated cost in USD. `None` means unknown, mock or local, never zero.
    pub estimated_usd: Option<f64>,
    /// Pricing status.
    pub cost_status: CostStatus,
}

/// `mcp_action` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpActionEvent {
    /// Task id.
    pub task_id: String,
    /// Step index within the task.
    pub step: u32,
    /// Tool called.
    pub tool_id: String,
    /// Call arguments.
    pub arguments: Map<String, Value>,
    /// Short summary of the result.
    pub result_summary: String,
    /// Whether the call errored.
    pub is_error: bool,
}

/// `canary_result` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanaryResultEvent {
    /// Canary status.
    pub status: CanaryStatus,
    /// Tool probed.
    pub tool_id: Option<String>,
    /// Probe id.
    pub probe_id: Option<String>,
    /// Whether the probe targets an unchanged control tool.
    pub is_control: bool,
    /// Expected observation.
    pub expected: Option<String>,
    /// Actual observation.
    pub actual: Option<String>,
    /// Sequence numbers of related events.
    pub event_refs: Vec<u64>,
}

/// `verifier_probe` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierProbeEvent {
    /// Task id.
    pub task_id: String,
    /// Probe id.
    pub probe_id: String,
    /// What the probe checks.
    pub description: String,
    /// What was observed.
    pub observation: String,
}

/// `verifier_verdict` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierVerdictEvent {
    /// Task id.
    pub task_id: String,
    /// Verifier outcome.
    pub outcome: VerifierOutcome,
    /// Whether the agent claimed completion.
    pub claimed_complete: bool,
    /// Explanation.
    pub rationale: String,
    /// Probes the verdict relied on.
    pub probe_ids: Vec<String>,
}

/// `metric_score` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricScoreEvent {
    /// Metric id.
    pub metric: Metric,
    /// Task id, or `None` for an aggregate.
    pub task_id: Option<String>,
    /// Score.
    pub value: f64,
}

/// `failure_category` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCategoryEvent {
    /// Task id.
    pub task_id: String,
    /// Failure category.
    pub category: FailureCategory,
    /// Detail text.
    pub detail: String,
}

/// `error` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEvent {
    /// Error message.
    pub message: String,
    /// Whether the run cannot continue.
    pub fatal: bool,
}

/// `run_complete` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCompleteEvent {
    /// Final status.
    pub status: RunStatus,
    /// Optional detail.
    pub detail: Option<String>,
}

/// Event payload, selected by `event_type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum EventBody {
    /// Run configuration.
    Config(ConfigEvent),
    /// Version diff.
    AdaptationDiff(AdaptationDiffEvent),
    /// Adaptation outcome.
    AdaptationOutcome(AdaptationOutcomeEvent),
    /// Model call usage/cost.
    ModelCall(ModelCallEvent),
    /// MCP tool action.
    McpAction(McpActionEvent),
    /// Canary result.
    CanaryResult(CanaryResultEvent),
    /// Verifier probe.
    VerifierProbe(VerifierProbeEvent),
    /// Verifier verdict.
    VerifierVerdict(VerifierVerdictEvent),
    /// Metric score.
    MetricScore(MetricScoreEvent),
    /// Failure category.
    FailureCategory(FailureCategoryEvent),
    /// Error.
    Error(ErrorEvent),
    /// Run completion.
    RunComplete(RunCompleteEvent),
}

impl EventBody {
    /// The `event_type` string of this body.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::AdaptationDiff(_) => "adaptation_diff",
            Self::AdaptationOutcome(_) => "adaptation_outcome",
            Self::ModelCall(_) => "model_call",
            Self::McpAction(_) => "mcp_action",
            Self::CanaryResult(_) => "canary_result",
            Self::VerifierProbe(_) => "verifier_probe",
            Self::VerifierVerdict(_) => "verifier_verdict",
            Self::MetricScore(_) => "metric_score",
            Self::FailureCategory(_) => "failure_category",
            Self::Error(_) => "error",
            Self::RunComplete(_) => "run_complete",
        }
    }
}

/// One run event: envelope plus body (flattened on the wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Must equal [`RUN_EVENT_SCHEMA_VERSION`].
    pub schema_version: String,
    /// Run id.
    pub run_id: String,
    /// Sequence number, monotonic per `(run_id, source)`.
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub ts: String,
    /// Writer of the event.
    pub source: EventSource,
    /// Strategy the event belongs to, when any.
    pub strategy: Option<Strategy>,
    /// Payload.
    #[serde(flatten)]
    pub body: EventBody,
}

impl RunEvent {
    /// JSON value of the event (object keys sorted).
    pub fn to_value(&self) -> Result<Value, EventError> {
        Ok(serde_json::to_value(self)?)
    }

    /// Single-line JSON of the event (object keys sorted, no trailing newline).
    pub fn to_json_line(&self) -> Result<String, EventError> {
        Ok(serde_json::to_string(&self.to_value()?)?)
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing and reading
// ---------------------------------------------------------------------------------------------

/// Parse and validate one JSONL line. `line_no` is 1-based and named in every error.
///
/// Order: blank check, JSON parse, supported `schema_version` (fails closed), schema
/// validation, typed decode.
pub fn parse_event_line(line_no: usize, line: &str) -> Result<RunEvent, EventError> {
    if line.trim().is_empty() {
        return Err(EventError::BlankLine { line: line_no });
    }
    let value: Value = serde_json::from_str(line).map_err(|e| EventError::BadJson {
        line: line_no,
        message: e.to_string(),
    })?;
    if let Some(found) = value.get("schema_version")
        && found.as_str() != Some(RUN_EVENT_SCHEMA_VERSION)
    {
        return Err(EventError::UnsupportedSchemaVersion {
            line: line_no,
            found: found.to_string(),
        });
    }
    schema::validate_run_event(&value).map_err(|errors| EventError::Schema {
        line: line_no,
        errors,
    })?;
    serde_json::from_value(value).map_err(|e| EventError::Decode {
        line: line_no,
        message: e.to_string(),
    })
}

/// Read a whole JSONL log. Fails on the first invalid line, naming its 1-based number.
pub fn read_events(path: impl AsRef<Path>) -> Result<Vec<RunEvent>, EventError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|e| EventError::io(path, e))?;
    text.lines()
        .enumerate()
        .map(|(i, line)| parse_event_line(i + 1, line))
        .collect()
}

/// Lenient read for partial logs: returns the valid prefix and the first error, if any.
pub fn read_events_partial(path: impl AsRef<Path>) -> (Vec<RunEvent>, Option<EventError>) {
    let path = path.as_ref();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return (Vec::new(), Some(EventError::io(path, e))),
    };
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        match parse_event_line(i + 1, line) {
            Ok(event) => events.push(event),
            Err(e) => return (events, Some(e)),
        }
    }
    (events, None)
}

// ---------------------------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------------------------

/// Destination for validated, redacted events.
pub trait EventSink {
    /// Consume one event.
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError>;
}

impl<S: EventSink + ?Sized> EventSink for Box<S> {
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError> {
        (**self).emit(event)
    }
}

/// Appends one JSON line per event to a file, flushing after every event.
#[derive(Debug)]
pub struct JsonlFileSink {
    path: PathBuf,
    file: File,
}

impl JsonlFileSink {
    /// Open `path` for appending, creating it (and parent directories) when missing.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, EventError> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| EventError::io(parent, e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| EventError::io(&path, e))?;
        Ok(Self { path, file })
    }

    /// Path being written.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl EventSink for JsonlFileSink {
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError> {
        let mut line = event.to_json_line()?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|e| EventError::io(&self.path, e))
    }
}

/// In-memory sink. Clones share the same buffer, so keep a clone to read events back after
/// handing the sink to an emitter.
#[derive(Debug, Clone, Default)]
pub struct VecSink {
    events: Arc<Mutex<Vec<RunEvent>>>,
}

impl VecSink {
    /// Empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the events received so far.
    pub fn events(&self) -> Vec<RunEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Number of events received so far.
    pub fn len(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no event has been received.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventSink for VecSink {
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());
        Ok(())
    }
}

/// Fans each event out to several sinks, in order. Stops at the first failing sink.
#[derive(Default)]
pub struct TeeSink {
    sinks: Vec<Box<dyn EventSink + Send>>,
}

impl TeeSink {
    /// Fan out to `sinks`.
    pub fn new(sinks: Vec<Box<dyn EventSink + Send>>) -> Self {
        Self { sinks }
    }

    /// Add one more sink.
    pub fn push(&mut self, sink: Box<dyn EventSink + Send>) {
        self.sinks.push(sink);
    }
}

impl EventSink for TeeSink {
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError> {
        self.sinks.iter_mut().try_for_each(|s| s.emit(event))
    }
}

/// Adapter turning a closure into a sink.
pub struct FnSink<F>(F);

impl<F> FnSink<F>
where
    F: FnMut(&RunEvent) -> Result<(), EventError>,
{
    /// Wrap `f`.
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> EventSink for FnSink<F>
where
    F: FnMut(&RunEvent) -> Result<(), EventError>,
{
    fn emit(&mut self, event: &RunEvent) -> Result<(), EventError> {
        (self.0)(event)
    }
}

// ---------------------------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------------------------

/// The single choke point for event output: stamps the envelope, redacts secrets, validates
/// against the schema and only then hands the event to the sink.
pub struct EventEmitter {
    run_id: String,
    source: EventSource,
    next_seq: u64,
    clock: Arc<dyn Clock>,
    redactor: Redactor,
    sink: Box<dyn EventSink + Send>,
}

impl EventEmitter {
    /// Create an emitter. Sequence numbers start at 0.
    pub fn new(
        run_id: impl Into<String>,
        source: EventSource,
        clock: Arc<dyn Clock>,
        redactor: Redactor,
        sink: Box<dyn EventSink + Send>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            source,
            next_seq: 0,
            clock,
            redactor,
            sink,
        }
    }

    /// Run id stamped on every event.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Sequence number the next emitted event will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Stamp, redact, validate and write one event; returns the event as written (redacted).
    ///
    /// The sequence number advances only when the event was written successfully.
    pub fn emit(
        &mut self,
        strategy: Option<Strategy>,
        body: EventBody,
    ) -> Result<RunEvent, EventError> {
        let event = RunEvent {
            schema_version: RUN_EVENT_SCHEMA_VERSION.to_owned(),
            run_id: self.run_id.clone(),
            seq: self.next_seq,
            ts: self.clock.now_rfc3339(),
            source: self.source,
            strategy,
            body,
        };
        let written = self.redact_validate_write(&event)?;
        self.next_seq += 1;
        Ok(written)
    }

    /// Pass an already-stamped event (for example a Python event) through redaction, schema
    /// validation and the sink without restamping it.
    pub fn forward(&mut self, event: &RunEvent) -> Result<RunEvent, EventError> {
        self.redact_validate_write(event)
    }

    fn redact_validate_write(&mut self, event: &RunEvent) -> Result<RunEvent, EventError> {
        let mut value = event.to_value()?;
        self.redactor.redact_json(&mut value);
        schema::validate_run_event(&value).map_err(|errors| EventError::Invalid { errors })?;
        let redacted: RunEvent =
            serde_json::from_value(value).map_err(|e| EventError::Invalid {
                errors: vec![e.to_string()],
            })?;
        self.sink.emit(&redacted)?;
        Ok(redacted)
    }
}
