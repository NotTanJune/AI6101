//! Core services for the MCP tool-knowledge adaptation harness.
//!
//! The TUI and the headless CLI both drive this crate. Modules owned by later
//! phases are declared here so `lib.rs` never needs to change again; each owner
//! adds its own `pub use` lines inside its module file.

pub mod canary;
pub mod clock;
pub mod config;
pub mod cost;
pub mod diff;
pub mod error;
pub mod events;
pub mod mcpevol;
pub mod model;
pub mod patch;
pub mod pilot;
pub mod pipeline;
pub mod provider;
pub mod redact;
pub mod runner;
pub mod schema;
pub mod source;
pub mod strategy;

pub use clock::{Clock, FixedClock, SystemClock};
pub use config::{Config, ConfigError, Problem, ProviderKind as ConfigProviderKind, Strategy};
pub use cost::{
    CostError, CostLedger, CostRecord, PhaseTotals, ProviderKind, RateCard, RateEntry, Reservation,
    ReserveRequest, Usage,
};
pub use error::{HarnessError, Result};
pub use events::{
    AdaptationDiffEvent, AdaptationOutcomeEvent, AdaptationStatus, CanaryResultEvent, CanaryStatus,
    ChangeKind, ChangeRecord, ConfigEvent, CostStatus, ErrorEvent, EventBody, EventEmitter,
    EventError, EventSink, EventSource, FailureCategory, FailureCategoryEvent, FnSink,
    JsonlFileSink, McpActionEvent, Metric, MetricScoreEvent, ModelCallEvent, Phase,
    RUN_EVENT_SCHEMA_VERSION, RunCompleteEvent, RunEvent, RunStatus, TeeSink, VecSink,
    VerifierOutcome, VerifierProbeEvent, VerifierVerdictEvent, parse_event_line, read_events,
    read_events_partial,
};
pub use mcpevol::{McpEvolAdapter, McpEvolOptions, McpEvolReport, StageLabels};
pub use model::{
    ChangeClass, Provenance, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb, VersionStage,
    canonical_json, card_hash, try_canonical_json,
};
pub use pipeline::{
    AdapterScope, EnvLookup, RunOptions, RunSelection, RunSummary, ValidationReport, build_adapter,
    run_matrix, validate_config_inputs,
};
pub use redact::Redactor;
