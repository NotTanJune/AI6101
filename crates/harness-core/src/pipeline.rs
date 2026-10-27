//! Run matrix orchestration, output layout and summary.
//!
//! [`run_matrix`] is the only pipeline: the headless `harness run` command and the TUI
//! controller both call it. For every repetition and every configured strategy it
//!
//! 1. emits a `config` event and the `adaptation_diff` between the source and target KB,
//! 2. adapts the KB through the strategy (all model calls metered on ONE shared ledger),
//! 3. validates the candidate against the source and target definitions,
//! 4. runs the canary gate (`not_applicable` for no-update) and decides promotion,
//! 5. writes the ACTIVE KB (candidate only when promoted, else the prior KB) to
//!    `kb/<strategy>-rep<N>.json`,
//! 6. checks the remaining local dispatch budget and runs the benchmark stage, forwarding the
//!    runner's events (source `python`) into the merged log and the cost ledger,
//!
//! and finally emits one Rust `run_complete`. The pipeline computes no benchmark scores: metric
//! events are passed through by id and value only.
//!
//! Output layout under [`RunOptions::output_dir`]:
//!
//! ```text
//! manifest.toml                      resolved config without secrets, input checksums, contracts
//! events.jsonl                       merged, redacted, schema-valid run events
//! summary.json                       per strategy and repetition results (see RunSummary)
//! kb/<strategy>-rep<N>.json          active KB handed to the runner (canonical JSON)
//! python/<strategy>-rep<N>.jsonl     raw runner lines, preserved by the runner
//! ```
//!
//! Python events keep their content and timestamps, but their `seq` is restamped so the merged
//! log stays monotonic per `(run_id, source)`; the raw per-stage logs keep the original numbers.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canary::{
    CanaryGate, ProbeExecutor, PromotionDecision, SimulatedTargetExecutor, decide_promotion,
};
use crate::clock::{Clock, SystemClock};
use crate::config::{
    AdapterKind, Config, ConfigError, Problem, ProviderKind as ConfigProviderKind, RunnerKind,
    Stages, Strategy,
};
use crate::cost::{CostError, CostLedger, CostRecord, PhaseTotals, RateCard};
use crate::diff::{KbDiff, diff_kbs};
use crate::error::{HarnessError, Result};
use crate::events::{
    AdaptationDiffEvent, AdaptationOutcomeEvent, AdaptationStatus, CanaryStatus, ConfigEvent,
    CostStatus, ErrorEvent, EventBody, EventEmitter, EventError, EventSink, EventSource,
    FailureCategory, JsonlFileSink, Metric, Phase, RUN_EVENT_SCHEMA_VERSION, RunCompleteEvent,
    RunEvent, RunStatus, TeeSink, VerifierOutcome,
};
use crate::mcpevol::{self, McpEvolAdapter, McpEvolOptions, StageLabels};
use crate::model::{TOOL_KB_SCHEMA_VERSION, ToolKb};
use crate::provider::{
    Decoding, MeteredError, MeteredProvider, Provider, build_provider_with_mock_dir,
};
use crate::redact::Redactor;
use crate::runner::{
    BenchmarkRunner, FixtureRunner, ProcessRunner, RunnerError, StageOutcome, StageRequest,
};
use crate::source::{FixtureAdapter, InputFile, Inventory, ServerInfo, SourceAdapter};
use crate::strategy::{
    AdaptationContext, Candidate, StrategyError, kb_hash, strategy_for,
    validate_candidate_against_source,
};

/// Format id written into `summary.json`.
pub const SUMMARY_FORMAT: &str = "harness-summary/1";

/// Wording that must accompany every cost figure.
pub const COST_NOTE: &str = "USD figures cover priced calls only. Mock calls carry no USD and \
calls with unknown usage or price are counted separately, never as zero. \
local_dispatch_cap_usd is a local dispatch guard, not the provider's bill.";

/// Environment lookup used for provider keys and dataset roots. Tests and the TUI pass an
/// override here; secrets never travel any other way.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Test hook: builds the canary executor for a strategy from the TARGET KB.
pub type ProbeExecutorFactory =
    Arc<dyn Fn(&ToolKb, Strategy) -> Box<dyn ProbeExecutor> + Send + Sync>;

/// Progress callback (strategy, text).
pub type ProgressFn = Box<dyn FnMut(Option<Strategy>, &str) + Send>;

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

/// Optional overrides so a front end can run a subset of the configured matrix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSelection {
    /// Task ids to run instead of `[tasks].ids`.
    pub task_ids: Option<Vec<String>>,
    /// Strategies to run instead of `[run].strategies`.
    pub strategies: Option<Vec<Strategy>>,
    /// Source version label override.
    pub source_version: Option<String>,
    /// Target version label override.
    pub target_version: Option<String>,
}

/// Everything [`run_matrix`] needs besides the config.
pub struct RunOptions {
    /// Directory receiving the run artifacts.
    pub output_dir: PathBuf,
    /// Time source for Rust-authored events.
    pub clock: Arc<dyn Clock>,
    /// Environment lookup (provider key, dataset roots).
    pub env: EnvLookup,
    /// Set to `true` to cancel; checked between steps and by the process runner.
    pub cancel: Arc<AtomicBool>,
    /// Extra event destination (for example a TUI channel). Its failures never stop the run.
    pub extra_sink: Option<Box<dyn EventSink + Send>>,
    /// Subset overrides.
    pub selection: RunSelection,
    /// Replace previous harness artifacts in `output_dir` instead of refusing.
    pub overwrite: bool,
    /// Test hook: canary executor factory (default: [`SimulatedTargetExecutor`]).
    pub probe_executor: Option<ProbeExecutorFactory>,
    /// Test hook: runner replacing the one built from `[runner]`.
    pub runner: Option<Box<dyn BenchmarkRunner>>,
    /// Test hook: provider replacing the one built from `[provider]`.
    pub provider: Option<Box<dyn Provider>>,
    /// Progress lines for a front end.
    pub progress: Option<ProgressFn>,
}

impl RunOptions {
    /// Options with the system clock, the process environment and a fresh cancel flag.
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            clock: Arc::new(SystemClock),
            env: Arc::new(|name| std::env::var(name).ok()),
            cancel: Arc::new(AtomicBool::new(false)),
            extra_sink: None,
            selection: RunSelection::default(),
            overwrite: false,
            probe_executor: None,
            runner: None,
            provider: None,
            progress: None,
        }
    }

    /// Use `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Use `env` for provider keys and dataset roots.
    #[must_use]
    pub fn with_env(mut self, env: EnvLookup) -> Self {
        self.env = env;
        self
    }

    /// Use `cancel` as the cancel flag.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Also send every event to `sink`.
    #[must_use]
    pub fn with_extra_sink(mut self, sink: Box<dyn EventSink + Send>) -> Self {
        self.extra_sink = Some(sink);
        self
    }

    /// Run a subset of the configured matrix.
    #[must_use]
    pub fn with_selection(mut self, selection: RunSelection) -> Self {
        self.selection = selection;
        self
    }

    /// Replace previous artifacts in the output directory.
    #[must_use]
    pub fn with_overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    /// Test hook: canary executor factory.
    #[must_use]
    pub fn with_probe_executor(mut self, factory: ProbeExecutorFactory) -> Self {
        self.probe_executor = Some(factory);
        self
    }

    /// Test hook: runner.
    #[must_use]
    pub fn with_runner(mut self, runner: Box<dyn BenchmarkRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    /// Test hook: provider.
    #[must_use]
    pub fn with_provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Receive progress lines.
    #[must_use]
    pub fn with_progress(mut self, progress: ProgressFn) -> Self {
        self.progress = Some(progress);
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Summary types
// ---------------------------------------------------------------------------------------------

/// How the benchmark stage of one strategy/repetition ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// The stage never started (adaptation refused, budget, cancel).
    NotRun,
    /// The runner finished normally.
    Completed,
    /// The runner stopped because the budget was exhausted.
    BudgetExhausted,
    /// The runner failed; partial events were kept.
    Failed,
    /// The stage was cancelled.
    Cancelled,
}

/// Adaptation result of one strategy/repetition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptationSummary {
    /// promoted, blocked, unchanged or rejected.
    pub status: AdaptationStatus,
    /// Reason recorded in the `adaptation_outcome` event.
    pub reason: String,
    /// Candidate hash, when a candidate existed.
    pub candidate_hash: Option<String>,
    /// Hash of the prior KB.
    pub prior_hash: String,
    /// Lineage ids the adaptation concerned.
    pub changed_lineage_ids: Vec<String>,
}

/// Verifier verdicts by outcome, counted from `verifier_verdict` events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictCounts {
    /// Confirmed successes.
    pub confirmed_success: u64,
    /// Confirmed failures.
    pub confirmed_failure: u64,
    /// Inconclusive verdicts.
    pub inconclusive: u64,
}

/// Failure categories counted from `failure_category` events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCounts {
    /// Under-update failures.
    pub under_update: u64,
    /// Over-update failures.
    pub over_update: u64,
}

/// One `metric_score` event, passed through unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricObservation {
    /// Metric id.
    pub metric: Metric,
    /// Task id, `None` for an aggregate reported by the runner.
    pub task_id: Option<String>,
    /// Value exactly as reported by the runner.
    pub value: f64,
}

/// One `failure_category` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureObservation {
    /// Task id.
    pub task_id: String,
    /// Category.
    pub category: FailureCategory,
}

/// Cost totals. USD covers priced calls only; unknown usage or price is never zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSummary {
    /// Model calls recorded (adaptation, canary and runner).
    pub calls: u64,
    /// Calls priced from usage and the rate card.
    pub priced_calls: u64,
    /// Calls with unknown usage or price (never counted as zero).
    pub unknown_calls: u64,
    /// Mock calls: mock usage, not USD.
    pub mock_calls: u64,
    /// Local-inference calls (tracked by latency, not USD).
    pub local_calls: u64,
    /// Input tokens where reported.
    pub input_tokens: u64,
    /// Output tokens where reported.
    pub output_tokens: u64,
    /// USD of priced calls; `None` when no call was priced. A lower bound whenever
    /// `unknown_calls > 0`.
    pub priced_usd: Option<f64>,
    /// Total latency of local-inference calls.
    pub local_latency_ms: u64,
    /// Totals by phase.
    pub by_phase: BTreeMap<Phase, PhaseTotals>,
    /// Configured local dispatch cap in USD. A local guard, NOT the provider's bill.
    pub local_dispatch_cap_usd: f64,
    /// Local dispatch budget left after this point.
    pub remaining_usd: f64,
    /// USD held for paid calls whose usage never came back (counts against the cap).
    pub unreconciled_usd: f64,
}

impl CostSummary {
    fn from_records(records: &[CostRecord], cap: f64, remaining: f64, unreconciled: f64) -> Self {
        let mut s = CostSummary {
            calls: 0,
            priced_calls: 0,
            unknown_calls: 0,
            mock_calls: 0,
            local_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            priced_usd: None,
            local_latency_ms: 0,
            by_phase: BTreeMap::new(),
            local_dispatch_cap_usd: cap,
            remaining_usd: remaining,
            unreconciled_usd: unreconciled,
        };
        for record in records {
            s.calls += 1;
            s.input_tokens += record.input_tokens.unwrap_or(0);
            s.output_tokens += record.output_tokens.unwrap_or(0);
            let phase = s.by_phase.entry(record.phase).or_default();
            phase.calls += 1;
            phase.input_tokens += record.input_tokens.unwrap_or(0);
            phase.output_tokens += record.output_tokens.unwrap_or(0);
            match record.cost_status {
                CostStatus::Priced => {
                    s.priced_calls += 1;
                    let usd = record.estimated_usd.unwrap_or(0.0);
                    s.priced_usd = Some(s.priced_usd.unwrap_or(0.0) + usd);
                    phase.usd += usd;
                }
                CostStatus::Unknown => {
                    s.unknown_calls += 1;
                    phase.unknown_calls += 1;
                }
                CostStatus::Mock => {
                    s.mock_calls += 1;
                    phase.mock_calls += 1;
                }
                CostStatus::Local => {
                    s.local_calls += 1;
                    s.local_latency_ms += record.latency_ms;
                    phase.local_latency_ms += record.latency_ms;
                }
            }
        }
        s
    }

    /// One-line human description that never hides mock or unknown usage.
    pub fn describe(&self) -> String {
        let usd = match self.priced_usd {
            Some(usd) if self.unknown_calls > 0 => {
                format!("USD at least ${usd:.6} (some calls unknown)")
            }
            Some(usd) => format!("USD ${usd:.6}"),
            None if self.unknown_calls > 0 => "USD unknown".to_owned(),
            None => "USD none priced".to_owned(),
        };
        format!(
            "{} call(s): {} priced, {} mock (mock usage, not USD), {} unknown, {} local; {usd}",
            self.calls, self.priced_calls, self.mock_calls, self.unknown_calls, self.local_calls
        )
    }
}

/// Result of one strategy in one repetition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategySummary {
    /// Strategy.
    pub strategy: Strategy,
    /// Repetition number (1-based).
    pub repetition: u32,
    /// Adaptation result; `None` when the run stopped before the outcome.
    pub adaptation: Option<AdaptationSummary>,
    /// Canary status; `None` when no canary ran (candidate rejected or run stopped).
    pub canary_status: Option<CanaryStatus>,
    /// Hash of the KB handed to the runner.
    pub active_kb_hash: Option<String>,
    /// Path of that KB relative to the output directory.
    pub active_kb_path: Option<String>,
    /// Benchmark stage result.
    pub stage: StageStatus,
    /// Verifier verdicts by outcome.
    pub verdicts: VerdictCounts,
    /// Metric events observed (id/value passthrough only; no score is computed here).
    pub metrics: Vec<MetricObservation>,
    /// Failure category events observed.
    pub failure_categories: Vec<FailureObservation>,
    /// Failure category counts.
    pub failure_counts: FailureCounts,
    /// Error messages recorded for this strategy.
    pub errors: Vec<String>,
    /// Cost totals for this strategy and repetition.
    pub cost: CostSummary,
}

impl StrategySummary {
    fn new(strategy: Strategy, repetition: u32, cap: f64) -> Self {
        Self {
            strategy,
            repetition,
            adaptation: None,
            canary_status: None,
            active_kb_hash: None,
            active_kb_path: None,
            stage: StageStatus::NotRun,
            verdicts: VerdictCounts::default(),
            metrics: Vec::new(),
            failure_categories: Vec::new(),
            failure_counts: FailureCounts::default(),
            errors: Vec::new(),
            cost: CostSummary::from_records(&[], cap, cap, 0.0),
        }
    }

    fn absorb(&mut self, event: &RunEvent) {
        match &event.body {
            EventBody::VerifierVerdict(v) => match v.outcome {
                VerifierOutcome::ConfirmedSuccess => self.verdicts.confirmed_success += 1,
                VerifierOutcome::ConfirmedFailure => self.verdicts.confirmed_failure += 1,
                VerifierOutcome::Inconclusive => self.verdicts.inconclusive += 1,
            },
            EventBody::MetricScore(m) => self.metrics.push(MetricObservation {
                metric: m.metric,
                task_id: m.task_id.clone(),
                value: m.value,
            }),
            EventBody::FailureCategory(f) => {
                match f.category {
                    FailureCategory::UnderUpdate => self.failure_counts.under_update += 1,
                    FailureCategory::OverUpdate => self.failure_counts.over_update += 1,
                }
                self.failure_categories.push(FailureObservation {
                    task_id: f.task_id.clone(),
                    category: f.category,
                });
            }
            EventBody::Error(e) => self.errors.push(e.message.clone()),
            _ => {}
        }
    }
}

/// Result of a whole run. Mirrors `summary.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Summary format id ([`SUMMARY_FORMAT`]).
    pub format: String,
    /// Run id (the configured run name).
    pub run_id: String,
    /// Final status, as in the Rust `run_complete` event.
    pub status: RunStatus,
    /// Detail recorded in the Rust `run_complete` event.
    pub detail: Option<String>,
    /// Source version label.
    pub source_version: String,
    /// Target version label.
    pub target_version: String,
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Judge version.
    pub judge_version: String,
    /// Experiment seed.
    pub seed: u64,
    /// Repetitions requested.
    pub repetitions: u32,
    /// Task ids of the run.
    pub task_ids: Vec<String>,
    /// One entry per strategy and repetition that was reached.
    pub entries: Vec<StrategySummary>,
    /// Totals over the whole run.
    pub cost: CostSummary,
    /// How to read the cost figures.
    pub cost_note: String,
    /// Output directory (not part of `summary.json`).
    #[serde(skip)]
    pub output_dir: PathBuf,
}

impl RunSummary {
    /// The entry for `strategy` in `repetition`.
    pub fn entry(&self, strategy: Strategy, repetition: u32) -> Option<&StrategySummary> {
        self.entries
            .iter()
            .find(|e| e.strategy == strategy && e.repetition == repetition)
    }
}

impl fmt::Display for RunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run {}: status {}",
            self.run_id,
            status_word(self.status)
        )?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        writeln!(f)?;
        writeln!(
            f,
            "versions {} -> {}; provider {} model {}; judge {}; {} task(s)",
            self.source_version,
            self.target_version,
            self.provider,
            self.model,
            self.judge_version,
            self.task_ids.len()
        )?;
        for e in &self.entries {
            writeln!(f, "strategy {} (repetition {})", e.strategy, e.repetition)?;
            match &e.adaptation {
                Some(a) => writeln!(
                    f,
                    "  adaptation: {} ({})",
                    adaptation_word(a.status),
                    a.reason
                )?,
                None => writeln!(f, "  adaptation: not reached")?,
            }
            writeln!(
                f,
                "  canary: {}",
                e.canary_status.map_or("not run", canary_word)
            )?;
            writeln!(f, "  runner stage: {}", stage_word(e.stage))?;
            writeln!(
                f,
                "  verdicts: confirmed_success {}, confirmed_failure {}, inconclusive {}",
                e.verdicts.confirmed_success, e.verdicts.confirmed_failure, e.verdicts.inconclusive
            )?;
            if !e.metrics.is_empty() {
                let metrics: Vec<String> = e
                    .metrics
                    .iter()
                    .map(|m| match &m.task_id {
                        Some(t) => format!("{}[{t}]={}", metric_word(m.metric), m.value),
                        None => format!("{}={}", metric_word(m.metric), m.value),
                    })
                    .collect();
                writeln!(f, "  metrics (as reported): {}", metrics.join(", "))?;
            }
            writeln!(
                f,
                "  failures: under_update {}, over_update {}",
                e.failure_counts.under_update, e.failure_counts.over_update
            )?;
            for error in &e.errors {
                writeln!(f, "  error: {error}")?;
            }
            writeln!(f, "  cost: {}", e.cost.describe())?;
        }
        writeln!(f, "total cost: {}", self.cost.describe())?;
        write!(
            f,
            "local dispatch cap ${:.2} (a local guard, not the provider bill); remaining ${:.2}",
            self.cost.local_dispatch_cap_usd, self.cost.remaining_usd
        )
    }
}

fn status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Ok => "ok",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::BudgetExhausted => "budget_exhausted",
    }
}

fn adaptation_word(status: AdaptationStatus) -> &'static str {
    match status {
        AdaptationStatus::Promoted => "promoted",
        AdaptationStatus::Blocked => "blocked",
        AdaptationStatus::Unchanged => "unchanged",
        AdaptationStatus::Rejected => "rejected",
    }
}

fn canary_word(status: CanaryStatus) -> &'static str {
    match status {
        CanaryStatus::Pass => "pass",
        CanaryStatus::Fail => "fail",
        CanaryStatus::Inconclusive => "inconclusive",
        CanaryStatus::NotApplicable => "not_applicable",
    }
}

fn stage_word(status: StageStatus) -> &'static str {
    match status {
        StageStatus::NotRun => "not run",
        StageStatus::Completed => "completed",
        StageStatus::BudgetExhausted => "budget_exhausted",
        StageStatus::Failed => "failed",
        StageStatus::Cancelled => "cancelled",
    }
}

fn metric_word(metric: Metric) -> &'static str {
    match metric {
        Metric::Tf => "tf",
        Metric::Grounding => "grounding",
        Metric::Pe => "pe",
        Metric::Ecs => "ecs",
        Metric::Recovery => "recovery",
        Metric::Retention => "retention",
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// What `harness validate` found. Built without any provider or network contact.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    /// Run name.
    pub run_name: String,
    /// Input adapter kind.
    pub adapter: String,
    /// Release id reported by the adapter.
    pub release_id: String,
    /// Servers with tool counts per version.
    pub servers: Vec<ServerInfo>,
    /// Adapter-specific lines (input hashes, counts, mappings, unresolved names).
    pub input_notes: Vec<String>,
    /// Task ids selected by the config.
    pub task_ids: Vec<String>,
    /// Tasks known to the adapter.
    pub inventory_task_count: usize,
    /// Source version label.
    pub source_version: String,
    /// Target version label.
    pub target_version: String,
    /// Changes detected between the source and target KB.
    pub detected_changes: usize,
    /// Strategies of the run.
    pub strategies: Vec<Strategy>,
    /// Repetitions.
    pub repetitions: u32,
    /// Experiment seed.
    pub seed: u64,
    /// Provider kind.
    pub provider_kind: ConfigProviderKind,
    /// Model id.
    pub provider_model: String,
    /// Judge version.
    pub judge_version: String,
    /// Runner kind.
    pub runner_kind: RunnerKind,
    /// Local dispatch cap in USD.
    pub budget_cap_usd: f64,
    /// Rate card as `(id, date, entries)`, when configured.
    pub rate_card: Option<(String, String, usize)>,
    /// Tool KB schema version this build reads and writes.
    pub tool_kb_schema_version: String,
    /// Run-event schema version this build reads and writes.
    pub run_event_schema_version: String,
    /// Things that will not stop the run but need attention.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    /// Number of tasks selected by the config.
    pub fn task_count(&self) -> usize {
        self.task_ids.len()
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "configuration OK (run \"{}\")", self.run_name)?;
        writeln!(
            f,
            "inputs: adapter {}, release {}",
            self.adapter, self.release_id
        )?;
        for line in &self.input_notes {
            writeln!(f, "{line}")?;
        }
        writeln!(f, "servers: {}", self.servers.len())?;
        for server in &self.servers {
            let versions: Vec<String> = server
                .versions
                .iter()
                .map(|v| format!("{} = {} tool(s)", v.label, v.tool_count))
                .collect();
            writeln!(f, "  {}: {}", server.server_id, versions.join(", "))?;
        }
        writeln!(
            f,
            "tasks: {} selected ({}); {} in the inventory",
            self.task_count(),
            self.task_ids.join(", "),
            self.inventory_task_count
        )?;
        writeln!(
            f,
            "versions: {} -> {} ({} change(s) detected)",
            self.source_version, self.target_version, self.detected_changes
        )?;
        let strategies: Vec<&str> = self.strategies.iter().map(|s| s.as_str()).collect();
        writeln!(
            f,
            "strategies: {} ({} repetition(s), seed {})",
            strategies.join(", "),
            self.repetitions,
            self.seed
        )?;
        writeln!(
            f,
            "provider: {} (model {})",
            self.provider_kind.name(),
            self.provider_model
        )?;
        writeln!(f, "judge: {}", self.judge_version)?;
        writeln!(
            f,
            "runner: {}",
            match self.runner_kind {
                RunnerKind::Fixture => "fixture",
                RunnerKind::Process => "process",
            }
        )?;
        match &self.rate_card {
            Some((id, date, n)) => writeln!(
                f,
                "budget: local dispatch cap ${:.2}; rate card {id} ({date}, {n} entr{})",
                self.budget_cap_usd,
                if *n == 1 { "y" } else { "ies" }
            )?,
            None => writeln!(
                f,
                "budget: local dispatch cap ${:.2}; no rate card (paid providers are blocked)",
                self.budget_cap_usd
            )?,
        }
        write!(
            f,
            "schema versions: {}, {}",
            self.tool_kb_schema_version, self.run_event_schema_version
        )?;
        for warning in &self.warnings {
            write!(f, "\nwarning: {warning}")?;
        }
        Ok(())
    }
}

struct Inspection {
    report: ValidationReport,
    problems: Vec<Problem>,
    adapter: Option<Box<dyn SourceAdapter>>,
}

/// How much of the inputs an adapter should expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterScope {
    /// Only what the configured `tasks.ids` need (runs and validation).
    ConfiguredTasks,
    /// Everything the inputs offer (the TUI inventory).
    All,
}

/// The ONE place that constructs the input adapter from a config. Returns every problem found
/// (missing env var, missing directory, missing file, unreadable inputs), never a partial
/// adapter. `validate`, `run_matrix` and the TUI inventory all call this.
pub fn build_adapter(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
    scope: AdapterScope,
) -> std::result::Result<Box<dyn SourceAdapter>, Vec<Problem>> {
    match config.inputs.adapter {
        AdapterKind::Fixture => {
            let Some(root) = config.inputs.root.as_ref().filter(|r| r.is_dir()) else {
                return Err(vec![problem(
                    "inputs.root",
                    "fixture root is missing or not a directory",
                )]);
            };
            FixtureAdapter::open(root)
                .map(|a| Box::new(a) as Box<dyn SourceAdapter>)
                .map_err(|e| vec![problem("inputs.root", e.to_string())])
        }
        AdapterKind::Mcpevol => {
            let mut problems = Vec::new();
            let mut dir_for = |field: &str, var: &str, required: Vec<String>| -> Option<PathBuf> {
                let Some(value) = env(var).filter(|v| !v.trim().is_empty()) else {
                    problems.push(problem(
                        field,
                        format!(
                            "environment variable {var} is not set; it must name an MCPEvol-Bench data directory containing {}",
                            required.join(", ")
                        ),
                    ));
                    return None;
                };
                let dir = PathBuf::from(value);
                if !dir.is_dir() {
                    problems.push(problem(
                        field,
                        format!(
                            "{var} does not name an existing directory ({})",
                            dir.display()
                        ),
                    ));
                    return None;
                }
                let missing = mcpevol::missing_files(&dir, &required);
                if !missing.is_empty() {
                    problems.push(problem(
                        field,
                        format!(
                            "{var} ({}) is missing: {}",
                            dir.display(),
                            missing.join(", ")
                        ),
                    ));
                    return None;
                }
                Some(dir)
            };
            let catalog = dir_for(
                "inputs.catalog_env",
                &config.inputs.catalog_env,
                mcpevol::required_catalog_files(),
            );
            let eval = dir_for(
                "inputs.eval_env",
                &config.inputs.eval_env,
                mcpevol::required_eval_files(),
            );
            let (Some(catalog), Some(eval)) = (catalog, eval) else {
                return Err(problems);
            };
            let defaults = StageLabels::default();
            let stages = config.versions.stages.as_ref();
            let options = McpEvolOptions {
                stages: StageLabels {
                    early: stages
                        .and_then(|s| s.early.clone())
                        .unwrap_or(defaults.early),
                    middle: stages
                        .and_then(|s| s.middle.clone())
                        .unwrap_or(defaults.middle),
                    late: stages.and_then(|s| s.late.clone()).unwrap_or(defaults.late),
                },
                scope_task_ids: match scope {
                    AdapterScope::ConfiguredTasks => Some(config.tasks.ids.clone()),
                    AdapterScope::All => None,
                },
                ..McpEvolOptions::default()
            };
            McpEvolAdapter::open(&catalog, &eval, options)
                .map(|a| Box::new(a) as Box<dyn SourceAdapter>)
                .map_err(|e| vec![problem("inputs.catalog_env", e.to_string())])
        }
    }
}

fn problem(field: &str, message: impl Into<String>) -> Problem {
    Problem {
        field: field.to_owned(),
        message: message.into(),
    }
}

fn inspect(config: &Config, env: &dyn Fn(&str) -> Option<String>) -> Inspection {
    let mut problems = config.validate();
    let mut warnings = Vec::new();
    let mut report = ValidationReport {
        run_name: config.run.name.clone(),
        adapter: match config.inputs.adapter {
            AdapterKind::Fixture => "fixture".to_owned(),
            AdapterKind::Mcpevol => "mcpevol".to_owned(),
        },
        release_id: String::new(),
        servers: Vec::new(),
        input_notes: Vec::new(),
        task_ids: config.tasks.ids.clone(),
        inventory_task_count: 0,
        source_version: config.versions.source.clone(),
        target_version: config.versions.target.clone(),
        detected_changes: 0,
        strategies: config.run.strategies.clone(),
        repetitions: config.run.repetitions,
        seed: config.run.seed,
        provider_kind: config.provider.kind,
        provider_model: config.provider.model.clone(),
        judge_version: config.judge.version.clone(),
        runner_kind: config.runner.kind,
        budget_cap_usd: config.budget.cap_usd,
        rate_card: config
            .budget
            .rate_card
            .as_ref()
            .map(|rc| (rc.id.clone(), rc.date.clone(), rc.entries.len())),
        tool_kb_schema_version: TOOL_KB_SCHEMA_VERSION.to_owned(),
        run_event_schema_version: RUN_EVENT_SCHEMA_VERSION.to_owned(),
        warnings: Vec::new(),
    };

    // `config.validate()` already reports a missing fixture root; do not repeat it.
    let mut adapter = None;
    let fixture_root_ok = config.inputs.adapter != AdapterKind::Fixture
        || config.inputs.root.as_ref().is_some_and(|r| r.is_dir());
    if fixture_root_ok {
        match build_adapter(config, env, AdapterScope::ConfiguredTasks) {
            Ok(opened) => {
                inspect_inputs(config, &*opened, &mut report, &mut problems);
                adapter = Some(opened);
            }
            Err(found) => problems.extend(found),
        }
    }

    inspect_provider_and_runner(config, env, &mut problems, &mut warnings);
    report.warnings = warnings;
    Inspection {
        report,
        problems,
        adapter,
    }
}

fn inspect_inputs(
    config: &Config,
    adapter: &dyn SourceAdapter,
    report: &mut ValidationReport,
    problems: &mut Vec<Problem>,
) {
    let inventory: Inventory = match adapter.inventory() {
        Ok(inventory) => inventory,
        Err(e) => {
            problems.push(problem("inputs", e.to_string()));
            return;
        }
    };
    report.release_id = inventory.release_id.clone();
    report.input_notes = adapter.describe();
    report.inventory_task_count = inventory.tasks.len();
    report.servers = inventory.servers.clone();

    for id in &config.tasks.ids {
        match inventory.tasks.iter().find(|t| &t.task_id == id) {
            None => problems.push(problem(
                "tasks.ids",
                format!("task {id:?} does not exist in the inputs"),
            )),
            Some(task) => {
                let server_known = inventory
                    .servers
                    .iter()
                    .any(|s| s.server_id == task.server_id);
                if !server_known {
                    problems.push(problem(
                        "tasks.ids",
                        format!("task {id:?} names unknown server {:?}", task.server_id),
                    ));
                }
            }
        }
    }

    let labels_owned = adapter.version_labels();
    let labels: Vec<&str> = labels_owned.iter().map(String::as_str).collect();
    let check_version = |field: &str, label: &str, problems: &mut Vec<Problem>| -> Option<ToolKb> {
        match adapter.load_kb(label) {
            Ok(kb) => Some(kb),
            Err(_) => {
                problems.push(problem(
                    field,
                    format!(
                        "version {label:?} does not exist (available: {})",
                        labels.join(", ")
                    ),
                ));
                None
            }
        }
    };
    let source = check_version("versions.source", &config.versions.source, problems);
    let target = check_version("versions.target", &config.versions.target, problems);
    if let (Some(source), Some(target)) = (&source, &target) {
        report.detected_changes = diff_kbs(source, target).changes.len();
    }
    if let Some(Stages {
        early,
        middle,
        late,
    }) = &config.versions.stages
    {
        for (field, label) in [
            ("versions.stages.early", early),
            ("versions.stages.middle", middle),
            ("versions.stages.late", late),
        ] {
            if let Some(label) = label
                && !labels.contains(&label.as_str())
            {
                problems.push(problem(
                    field,
                    format!(
                        "version {label:?} does not exist (available: {})",
                        labels.join(", ")
                    ),
                ));
            }
        }
    }
}

fn inspect_provider_and_runner(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
    problems: &mut Vec<Problem>,
    warnings: &mut Vec<String>,
) {
    let uses_model = config
        .run
        .strategies
        .iter()
        .any(|s| *s != Strategy::NoUpdate);
    if config.provider.kind == ConfigProviderKind::Mock && uses_model {
        match &config.provider.mock_responses_dir {
            None => problems.push(problem(
                "provider.mock_responses_dir",
                "the mock provider needs mock_responses_dir (a directory of <purpose>.json responses) when full_rewrite or selective_patch runs",
            )),
            Some(dir) if dir.is_dir() => {
                for strategy in &config.run.strategies {
                    if *strategy == Strategy::NoUpdate {
                        continue;
                    }
                    let file = dir.join(format!("{}.json", strategy.as_str()));
                    if !file.is_file() {
                        problems.push(problem(
                            "provider.mock_responses_dir",
                            format!("missing mock response {}", file.display()),
                        ));
                    }
                }
            }
            Some(_) => {}
        }
    }
    if let Some(name) = &config.provider.api_key_env
        && env(name).is_none_or(|v| v.trim().is_empty())
    {
        let needed = matches!(
            config.provider.kind,
            ConfigProviderKind::OpenAi | ConfigProviderKind::Gemini
        );
        if needed {
            warnings.push(format!(
                "environment variable {name} is not set; the run cannot authenticate to {}",
                config.provider.kind.name()
            ));
        }
    }
    if config.runner.kind == RunnerKind::Fixture
        && let Some(dir) = config.runner.fixture_dir.as_ref().filter(|d| d.is_dir())
    {
        for strategy in &config.run.strategies {
            let file = dir.join(format!("{}.jsonl", strategy.as_str()));
            if !file.is_file() {
                problems.push(problem(
                    "runner.fixture_dir",
                    format!("missing fixture log {}", file.display()),
                ));
            }
        }
    }
}

/// Check a config and its inputs without contacting any provider or network. Returns the
/// report, or `HarnessError::Config(ConfigError::Invalid { problems })` listing every problem.
pub fn validate_config_inputs(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ValidationReport> {
    let inspection = inspect(config, env);
    if inspection.problems.is_empty() {
        Ok(inspection.report)
    } else {
        Err(ConfigError::Invalid {
            problems: inspection.problems,
        }
        .into())
    }
}

// ---------------------------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct Manifest {
    contracts: ManifestContracts,
    harness: ManifestHarness,
    run: ManifestRun,
    versions: ManifestVersions,
    tasks: ManifestTasks,
    inputs: ManifestInputs,
    provider: ManifestProvider,
    judge: ManifestJudge,
    budget: ManifestBudget,
    runner: ManifestRunner,
    input_files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestContracts {
    tool_kb: String,
    run_event: String,
    run_summary: String,
}

#[derive(Serialize)]
struct ManifestHarness {
    version: String,
}

#[derive(Serialize)]
struct ManifestRun {
    name: String,
    seed: u64,
    repetitions: u32,
    strategies: Vec<String>,
}

#[derive(Serialize)]
struct ManifestVersions {
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stages: Option<Stages>,
}

#[derive(Serialize)]
struct ManifestTasks {
    ids: Vec<String>,
}

#[derive(Serialize)]
struct ManifestInputs {
    adapter: String,
    release_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    catalog_env: String,
    eval_env: String,
}

#[derive(Serialize)]
struct ManifestProvider {
    kind: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Environment variable NAME only; the value is never recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mock_responses_dir: Option<String>,
}

#[derive(Serialize)]
struct ManifestJudge {
    version: String,
}

#[derive(Serialize)]
struct ManifestBudget {
    /// Local dispatch cap, not the provider's bill.
    local_dispatch_cap_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_card: Option<RateCard>,
}

#[derive(Serialize)]
struct ManifestRunner {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fixture_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executable: Option<String>,
    fixed_args: Vec<String>,
}

#[derive(Serialize)]
struct ManifestFile {
    group: String,
    path: String,
    sha256: String,
}

fn relative_text(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_files(
    dir: &Path,
    prefix: &Path,
    group: &str,
    out: &mut Vec<ManifestFile>,
) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| HarnessError::io(dir, e))?
        .map(|e| e.map(|e| e.path()).map_err(|e| HarnessError::io(dir, e)))
        .collect::<Result<_>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, prefix, group, out)?;
        } else if path.is_file() {
            let bytes = fs::read(&path).map_err(|e| HarnessError::io(&path, e))?;
            out.push(ManifestFile {
                group: group.to_owned(),
                path: relative_text(prefix, &path),
                sha256: hex::encode(Sha256::digest(&bytes)),
            });
        }
    }
    Ok(())
}

fn build_manifest(
    config: &Config,
    release_id: &str,
    adapter_files: &[InputFile],
) -> Result<Manifest> {
    let base = &config.base_dir;
    let rel = |p: &Path| relative_text(base, p);
    let mut input_files = Vec::new();
    if !adapter_files.is_empty() {
        // Adapter-reported inputs: source-relative names, never absolute paths.
        input_files.extend(adapter_files.iter().map(|f| ManifestFile {
            group: "inputs".to_owned(),
            path: f.path.clone(),
            sha256: f.sha256.clone(),
        }));
    } else if let Some(root) = config.inputs.root.as_ref().filter(|r| r.is_dir()) {
        collect_files(root, root, "inputs", &mut input_files)?;
    }
    let outside_root = |dir: &Path| {
        config
            .inputs
            .root
            .as_ref()
            .is_none_or(|root| !dir.starts_with(root))
    };
    if config.provider.kind == ConfigProviderKind::Mock
        && let Some(dir) = config
            .provider
            .mock_responses_dir
            .as_ref()
            .filter(|d| d.is_dir() && outside_root(d))
    {
        collect_files(dir, dir, "mock_responses", &mut input_files)?;
    }
    if config.runner.kind == RunnerKind::Fixture
        && let Some(dir) = config
            .runner
            .fixture_dir
            .as_ref()
            .filter(|d| d.is_dir() && outside_root(d))
    {
        collect_files(dir, dir, "runner_fixtures", &mut input_files)?;
    }
    Ok(Manifest {
        contracts: ManifestContracts {
            tool_kb: TOOL_KB_SCHEMA_VERSION.to_owned(),
            run_event: RUN_EVENT_SCHEMA_VERSION.to_owned(),
            run_summary: SUMMARY_FORMAT.to_owned(),
        },
        harness: ManifestHarness {
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        run: ManifestRun {
            name: config.run.name.clone(),
            seed: config.run.seed,
            repetitions: config.run.repetitions,
            strategies: config
                .run
                .strategies
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
        },
        versions: ManifestVersions {
            source: config.versions.source.clone(),
            target: config.versions.target.clone(),
            stages: config.versions.stages.clone(),
        },
        tasks: ManifestTasks {
            ids: config.tasks.ids.clone(),
        },
        inputs: ManifestInputs {
            adapter: match config.inputs.adapter {
                AdapterKind::Fixture => "fixture".to_owned(),
                AdapterKind::Mcpevol => "mcpevol".to_owned(),
            },
            release_id: release_id.to_owned(),
            root: config.inputs.root.as_deref().map(&rel),
            catalog_env: config.inputs.catalog_env.clone(),
            eval_env: config.inputs.eval_env.clone(),
        },
        provider: ManifestProvider {
            kind: config.provider.kind.name().to_owned(),
            model: config.provider.model.clone(),
            base_url: config.provider.base_url.clone(),
            api_key_env: config.provider.api_key_env.clone(),
            temperature: config.provider.temperature,
            top_p: config.provider.top_p,
            max_output_tokens: config.provider.max_output_tokens,
            seed: config.provider.seed,
            mock_responses_dir: config.provider.mock_responses_dir.as_deref().map(&rel),
        },
        judge: ManifestJudge {
            version: config.judge.version.clone(),
        },
        budget: ManifestBudget {
            local_dispatch_cap_usd: config.budget.cap_usd,
            rate_card: config.budget.rate_card.clone(),
        },
        runner: ManifestRunner {
            kind: match config.runner.kind {
                RunnerKind::Fixture => "fixture".to_owned(),
                RunnerKind::Process => "process".to_owned(),
            },
            fixture_dir: config.runner.fixture_dir.as_deref().map(&rel),
            executable: config
                .runner
                .executable
                .as_deref()
                .map(|e| rel(Path::new(e))),
            fixed_args: config.runner.fixed_args.clone(),
        },
        input_files,
    })
}

// ---------------------------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------------------------

/// Forwards to another sink and swallows its failures, so a closed front end never stops a run.
struct BestEffortSink {
    inner: Box<dyn EventSink + Send>,
    failed: bool,
}

impl EventSink for BestEffortSink {
    fn emit(&mut self, event: &RunEvent) -> std::result::Result<(), EventError> {
        if !self.failed && self.inner.emit(event).is_err() {
            self.failed = true;
        }
        Ok(())
    }
}

enum Flow {
    Continue,
    Stop(RunStatus, String),
}

struct Pipeline {
    config: Config,
    output: PathBuf,
    run_id: String,
    source_kb: ToolKb,
    target_kb: ToolKb,
    diff: KbDiff,
    emitter: EventEmitter,
    redactor: Redactor,
    ledger: CostLedger,
    provider: Box<dyn Provider>,
    runner: Box<dyn BenchmarkRunner>,
    decoding: Decoding,
    probe_factory: Option<ProbeExecutorFactory>,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressFn>,
    /// Sequence counter for python events in the merged log.
    py_seq: u64,
    /// First harness-level failure (provider error, runner failure, invalid cost record).
    failure: Option<String>,
    /// Adaptations that ended blocked or rejected.
    not_promoted: u32,
}

const ARTIFACTS: [&str; 5] = [
    "events.jsonl",
    "manifest.toml",
    "summary.json",
    "kb",
    "python",
];

fn prepare_output(dir: &Path, overwrite: bool) -> Result<()> {
    if dir.exists() && !dir.is_dir() {
        return Err(HarnessError::Pipeline(format!(
            "output path {} exists and is not a directory",
            dir.display()
        )));
    }
    if dir.join("events.jsonl").exists() {
        if !overwrite {
            return Err(HarnessError::Pipeline(format!(
                "output directory {} already holds a run (events.jsonl exists); choose a new directory or allow overwriting",
                dir.display()
            )));
        }
        for name in ARTIFACTS {
            let path = dir.join(name);
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else if path.exists() {
                fs::remove_file(&path)
            } else {
                Ok(())
            };
            removed.map_err(|e| HarnessError::io(&path, e))?;
        }
    }
    for sub in ["", "kb", "python"] {
        let path = dir.join(sub);
        fs::create_dir_all(&path).map_err(|e| HarnessError::io(&path, e))?;
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|e| HarnessError::io(path, e))
}

fn effective_config(config: &Config, selection: &RunSelection) -> Config {
    let mut c = config.clone();
    if let Some(ids) = &selection.task_ids {
        c.tasks.ids = ids.clone();
    }
    if let Some(strategies) = &selection.strategies {
        c.run.strategies = strategies.clone();
    }
    if let Some(source) = &selection.source_version {
        c.versions.source = source.clone();
    }
    if let Some(target) = &selection.target_version {
        c.versions.target = target.clone();
    }
    c
}

fn build_default_runner(
    config: &Config,
    redactor: &Redactor,
    env: &EnvLookup,
    cancel: &Arc<AtomicBool>,
) -> Result<Box<dyn BenchmarkRunner>> {
    match config.runner.kind {
        RunnerKind::Fixture => {
            let dir = config.runner.fixture_dir.clone().ok_or_else(|| {
                RunnerError::InvalidRequest(
                    "runner.kind = \"fixture\" needs fixture_dir".to_owned(),
                )
            })?;
            Ok(Box::new(
                FixtureRunner::new(dir).with_redactor(redactor.clone()),
            ))
        }
        RunnerKind::Process => {
            let executable = config
                .runner
                .executable
                .clone()
                .filter(|e| !e.trim().is_empty())
                .ok_or_else(|| {
                    RunnerError::InvalidRequest(
                        "runner.kind = \"process\" needs executable".to_owned(),
                    )
                })?;
            let mut passthrough = vec![
                config.inputs.catalog_env.clone(),
                config.inputs.eval_env.clone(),
            ];
            passthrough.extend(config.provider.api_key_env.clone());
            let env = Arc::clone(env);
            let mut runner = ProcessRunner::new(executable, config.runner.fixed_args.clone())
                .with_passthrough_env(passthrough)
                .with_redactor(redactor.clone())
                .with_env_lookup(move |name| env(name))
                .with_cancel_flag(Arc::clone(cancel));
            if let Some(base_url) = &config.provider.base_url {
                runner = runner.with_env("MODEL_URL", base_url.clone());
            }
            runner = runner.with_env("PYTHON_DOTENV_DISABLED", "1");
            Ok(Box::new(runner))
        }
    }
}

/// Run the configured strategy/repetition matrix. See the module docs for the stages, the
/// output layout and the status rules.
///
/// Setup failures (invalid config or inputs, provider or runner cannot be built, output
/// directory in use) return `Err` before any file is written. Once the run has started, every
/// outcome (including failed stages, blocked candidates, budget exhaustion and cancellation)
/// is recorded in `events.jsonl` and `summary.json` and returned as `Ok`; check
/// [`RunSummary::status`].
pub fn run_matrix(config: &Config, opts: RunOptions) -> Result<RunSummary> {
    let RunOptions {
        output_dir,
        clock,
        env,
        cancel,
        extra_sink,
        selection,
        overwrite,
        probe_executor,
        runner,
        provider,
        progress,
    } = opts;

    let config = effective_config(config, &selection);
    let inspection = inspect(&config, &*env);
    if !inspection.problems.is_empty() {
        return Err(ConfigError::Invalid {
            problems: inspection.problems,
        }
        .into());
    }
    let release_id = inspection.report.release_id.clone();
    let adapter = inspection
        .adapter
        .ok_or_else(|| HarnessError::Pipeline("no input adapter could be opened".to_owned()))?;
    let source_kb = adapter.load_kb(&config.versions.source)?;
    let target_kb = adapter.load_kb(&config.versions.target)?;
    let diff = diff_kbs(&source_kb, &target_kb);

    let mut secrets = Vec::new();
    if let Some(name) = &config.provider.api_key_env
        && let Some(value) = env(name)
    {
        secrets.push(value);
    }
    let redactor = Redactor::new(secrets);

    let provider: Box<dyn Provider> = match provider {
        Some(p) => p,
        None => build_provider_with_mock_dir(
            &config.provider,
            &*env,
            config.provider.mock_responses_dir.as_deref(),
        )?,
    };
    let runner = match runner {
        Some(r) => r,
        None => build_default_runner(&config, &redactor, &env, &cancel)?,
    };
    let manifest = build_manifest(&config, &release_id, &adapter.input_files())?;

    // Everything fallible that needs no output has run; from here the run leaves artifacts.
    prepare_output(&output_dir, overwrite)?;
    let manifest_text = toml::to_string(&manifest)
        .map_err(|e| HarnessError::Pipeline(format!("cannot serialize the manifest: {e}")))?;
    write_file(
        &output_dir.join("manifest.toml"),
        redactor.redact_str(&manifest_text).as_bytes(),
    )?;

    let mut tee = TeeSink::new(vec![Box::new(JsonlFileSink::open(
        output_dir.join("events.jsonl"),
    )?)]);
    if let Some(extra) = extra_sink {
        tee.push(Box::new(BestEffortSink {
            inner: extra,
            failed: false,
        }));
    }
    let run_id = config.run.name.clone();
    let emitter = EventEmitter::new(
        run_id.clone(),
        EventSource::Rust,
        clock,
        redactor.clone(),
        Box::new(tee),
    );
    let rate_card = config
        .budget
        .rate_card
        .clone()
        .unwrap_or_else(|| RateCard::empty("none", "1970-01-01"));
    let ledger = CostLedger::new(config.budget.cap_usd, rate_card);
    let decoding = Decoding::from_section(&config.provider);

    let mut pipeline = Pipeline {
        config,
        output: output_dir,
        run_id,
        source_kb,
        target_kb,
        diff,
        emitter,
        redactor,
        ledger,
        provider,
        runner,
        decoding,
        probe_factory: probe_executor,
        cancel,
        progress,
        py_seq: 0,
        failure: None,
        not_promoted: 0,
    };
    pipeline.run()
}

impl Pipeline {
    fn say(&mut self, strategy: Option<Strategy>, text: &str) {
        if let Some(progress) = self.progress.as_mut() {
            progress(strategy, text);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn fail(&mut self, reason: String) {
        if self.failure.is_none() {
            self.failure = Some(reason);
        }
    }

    fn emit(&mut self, strategy: Option<Strategy>, body: EventBody) -> Result<RunEvent> {
        Ok(self.emitter.emit(strategy, body)?)
    }

    fn error_event(
        &mut self,
        strategy: Strategy,
        message: String,
        fatal: bool,
        entry: &mut StrategySummary,
    ) -> Result<()> {
        let written = self.emit(
            Some(strategy),
            EventBody::Error(ErrorEvent { message, fatal }),
        )?;
        entry.absorb(&written);
        Ok(())
    }

    fn run(&mut self) -> Result<RunSummary> {
        let cap = self.config.budget.cap_usd;
        let strategies = self.config.run.strategies.clone();
        let mut entries: Vec<StrategySummary> = Vec::new();
        let mut stop: Option<(RunStatus, String)> = None;

        'matrix: for repetition in 1..=self.config.run.repetitions {
            for &strategy in &strategies {
                if self.cancelled() {
                    stop = Some((RunStatus::Cancelled, "cancelled by the operator".to_owned()));
                    break 'matrix;
                }
                let mut entry = StrategySummary::new(strategy, repetition, cap);
                let start = self.ledger.records().len();
                let flow = self.run_one(repetition, strategy, &mut entry);
                entry.cost = CostSummary::from_records(
                    &self.ledger.records()[start..],
                    cap,
                    self.ledger.remaining_usd(),
                    self.ledger.unreconciled_usd(),
                );
                entries.push(entry);
                match flow? {
                    Flow::Continue => {}
                    Flow::Stop(status, detail) => {
                        stop = Some((status, detail));
                        break 'matrix;
                    }
                }
            }
        }

        let (status, detail) = match (stop, self.failure.clone()) {
            (Some((status @ (RunStatus::Cancelled | RunStatus::BudgetExhausted), detail)), _) => {
                (status, Some(detail))
            }
            (_, Some(reason)) => (RunStatus::Failed, Some(reason)),
            (_, None) if self.not_promoted > 0 => (
                RunStatus::Ok,
                Some(format!(
                    "{} adaptation(s) ended blocked or rejected; the prior knowledge base was used",
                    self.not_promoted
                )),
            ),
            (_, None) => (RunStatus::Ok, None),
        };
        self.emit(
            None,
            EventBody::RunComplete(RunCompleteEvent {
                status,
                detail: detail.clone(),
            }),
        )?;
        self.say(None, &format!("run complete: {}", status_word(status)));

        let summary = RunSummary {
            format: SUMMARY_FORMAT.to_owned(),
            run_id: self.run_id.clone(),
            status,
            detail,
            source_version: self.config.versions.source.clone(),
            target_version: self.config.versions.target.clone(),
            provider: self.config.provider.kind.name().to_owned(),
            model: self.config.provider.model.clone(),
            judge_version: self.config.judge.version.clone(),
            seed: self.config.run.seed,
            repetitions: self.config.run.repetitions,
            task_ids: self.config.tasks.ids.clone(),
            entries,
            cost: CostSummary::from_records(
                self.ledger.records(),
                cap,
                self.ledger.remaining_usd(),
                self.ledger.unreconciled_usd(),
            ),
            cost_note: COST_NOTE.to_owned(),
            output_dir: self.output.clone(),
        };
        let mut text = serde_json::to_string_pretty(&summary)?;
        text.push('\n');
        // The emitter's redactor guards events; the summary gets the same treatment.
        write_file(
            &self.output.join("summary.json"),
            self.redactor.redact_str(&text).as_bytes(),
        )?;
        Ok(summary)
    }

    fn run_one(
        &mut self,
        repetition: u32,
        strategy: Strategy,
        entry: &mut StrategySummary,
    ) -> Result<Flow> {
        let source_version = self.config.versions.source.clone();
        let target_version = self.config.versions.target.clone();
        self.say(
            Some(strategy),
            &format!("repetition {repetition}: {strategy}: starting"),
        );

        let cfg = &self.config;
        self.emit(
            Some(strategy),
            EventBody::Config(ConfigEvent {
                provider: cfg.provider.kind.name().to_owned(),
                model: cfg.provider.model.clone(),
                judge_version: cfg.judge.version.clone(),
                seed: cfg.run.seed,
                repetition,
                budget_cap_usd: Some(cfg.budget.cap_usd),
                task_ids: cfg.tasks.ids.clone(),
                source_version: source_version.clone(),
                target_version: target_version.clone(),
                temperature: cfg.provider.temperature,
                top_p: cfg.provider.top_p,
                max_output_tokens: cfg.provider.max_output_tokens,
            }),
        )?;
        self.emit(
            Some(strategy),
            EventBody::AdaptationDiff(AdaptationDiffEvent {
                source_version,
                target_version: target_version.clone(),
                changes: self.diff.changes.clone(),
                candidate_hash: None,
            }),
        )?;
        if self.cancelled() {
            return Ok(cancel_flow());
        }

        // Adaptation.
        self.say(Some(strategy), "adapting the knowledge base");
        let prior_hash = kb_hash(&self.source_kb);
        let candidate = match self.adapt(strategy) {
            Adapted::Candidate(candidate) => candidate,
            Adapted::Failed(failed) => {
                return self.adaptation_failed(strategy, prior_hash, failed, entry);
            }
            Adapted::Fatal(e) => return Err(e),
        };
        if self.cancelled() {
            return Ok(cancel_flow());
        }

        // Canary and promotion.
        let (report_status, decision, reason) = if strategy == Strategy::NoUpdate {
            let report = CanaryGate::not_applicable(&mut self.emitter, strategy)?;
            let decision = decide_promotion(&report, &self.source_kb, &candidate.kb);
            (
                report.status,
                decision,
                "no-update strategy: the knowledge base is left unchanged".to_owned(),
            )
        } else {
            self.say(Some(strategy), "running the canary gate");
            let mut executor: Box<dyn ProbeExecutor> = match &self.probe_factory {
                Some(factory) => factory(&self.target_kb, strategy),
                None => Box::new(SimulatedTargetExecutor::from_target_kb(&self.target_kb)),
            };
            let report = CanaryGate::run(
                &candidate.kb,
                &candidate.changes,
                Some(&self.target_kb),
                executor.as_mut(),
                &mut self.emitter,
                strategy,
            )?;
            let decision = decide_promotion(&report, &self.source_kb, &candidate.kb);
            let reason = match &decision {
                PromotionDecision::Promote(_) => {
                    format!("{}; candidate promoted", report.summary())
                }
                PromotionDecision::Blocked { reason, .. } => {
                    format!("{reason}; prior knowledge base kept")
                }
                PromotionDecision::Unchanged { .. } => {
                    "no candidate: the knowledge base is left unchanged".to_owned()
                }
            };
            (report.status, decision, reason)
        };
        entry.canary_status = Some(report_status);
        let status = decision.adaptation_status();
        if status == AdaptationStatus::Blocked {
            self.not_promoted += 1;
        }
        let outcome = AdaptationOutcomeEvent {
            status,
            reason: reason.clone(),
            candidate_hash: (status != AdaptationStatus::Unchanged)
                .then(|| candidate.candidate_hash.clone()),
            prior_hash: candidate.prior_hash.clone(),
            changed_lineage_ids: candidate.changed_lineage_ids.clone(),
        };
        entry.adaptation = Some(AdaptationSummary {
            status,
            reason,
            candidate_hash: outcome.candidate_hash.clone(),
            prior_hash: outcome.prior_hash.clone(),
            changed_lineage_ids: outcome.changed_lineage_ids.clone(),
        });
        self.emit(Some(strategy), EventBody::AdaptationOutcome(outcome))?;

        // Active KB for the runner: the promoted candidate or the prior KB, never a blocked one.
        let active = decision.active_kb().clone();
        self.run_stage(repetition, strategy, &active, entry)
    }

    fn adaptation_failed(
        &mut self,
        strategy: Strategy,
        prior_hash: String,
        failed: FailedAdaptation,
        entry: &mut StrategySummary,
    ) -> Result<Flow> {
        let changed: Vec<String> = self.diff.affected_lineage_ids.iter().cloned().collect();
        let reason = failed.message.clone();
        entry.adaptation = Some(AdaptationSummary {
            status: failed.status,
            reason: reason.clone(),
            candidate_hash: None,
            prior_hash: prior_hash.clone(),
            changed_lineage_ids: changed.clone(),
        });
        self.emit(
            Some(strategy),
            EventBody::AdaptationOutcome(AdaptationOutcomeEvent {
                status: failed.status,
                reason,
                candidate_hash: None,
                prior_hash,
                changed_lineage_ids: changed,
            }),
        )?;
        self.not_promoted += 1;
        self.error_event(
            strategy,
            format!("adaptation failed for {strategy}: {}", failed.message),
            failed.budget_stop,
            entry,
        )?;
        if failed.harness_failure {
            self.fail(format!(
                "adaptation failed for {strategy}: {}",
                failed.message
            ));
        }
        if failed.budget_stop {
            return Ok(Flow::Stop(
                RunStatus::BudgetExhausted,
                format!("local dispatch budget exhausted before the {strategy} adaptation"),
            ));
        }
        // The prior KB stays active. There is no candidate, so no canary runs; the runner
        // stage still records how the unchanged KB performs under this strategy.
        let prior = self.source_kb.clone();
        let repetition = entry.repetition;
        self.run_stage(repetition, strategy, &prior, entry)
    }

    fn adapt(&mut self, strategy: Strategy) -> Adapted {
        let strat = strategy_for(strategy);
        let result = {
            let mut metered = MeteredProvider::new(
                self.provider.as_mut(),
                &mut self.ledger,
                &mut self.emitter,
                Some(strategy),
            )
            .with_decoding(self.decoding.clone());
            let mut ctx = AdaptationContext {
                source: &self.source_kb,
                target: &self.target_kb,
                diff: &self.diff,
                provider: &mut metered,
            };
            strat.adapt(&mut ctx)
        };
        let result = result.and_then(|candidate| {
            validate_candidate_against_source(
                &self.source_kb,
                &candidate.kb,
                &self.target_kb,
                &self.diff,
                strategy,
            )
            .map(|()| candidate)
        });
        match result {
            Ok(candidate) => Adapted::Candidate(candidate),
            Err(StrategyError::Provider(MeteredError::Event(e))) => Adapted::Fatal(e.into()),
            Err(StrategyError::Provider(MeteredError::Refused(CostError::CapExceeded {
                requested_usd,
                remaining_usd,
                cap_usd,
            }))) => Adapted::Failed(FailedAdaptation {
                status: AdaptationStatus::Blocked,
                message: format!(
                    "dispatch refused: reservation of ${requested_usd:.6} would exceed the local dispatch cap of ${cap_usd:.2} (remaining ${remaining_usd:.6})"
                ),
                budget_stop: true,
                harness_failure: false,
            }),
            Err(e @ StrategyError::Provider(_)) => Adapted::Failed(FailedAdaptation {
                status: AdaptationStatus::Blocked,
                message: e.to_string(),
                budget_stop: false,
                harness_failure: true,
            }),
            Err(e) => Adapted::Failed(FailedAdaptation {
                status: AdaptationStatus::Rejected,
                message: e.to_string(),
                budget_stop: false,
                harness_failure: false,
            }),
        }
    }

    fn run_stage(
        &mut self,
        repetition: u32,
        strategy: Strategy,
        active: &ToolKb,
        entry: &mut StrategySummary,
    ) -> Result<Flow> {
        if self.cancelled() {
            return Ok(cancel_flow());
        }
        let kb_rel = format!("kb/{}-rep{repetition}.json", strategy.as_str());
        let kb_bytes = active.canonical_bytes();
        write_file(&self.output.join(&kb_rel), &kb_bytes)?;
        entry.active_kb_hash = Some(hex::encode(Sha256::digest(&kb_bytes)));
        entry.active_kb_path = Some(kb_rel.clone());

        let remaining = self.ledger.remaining_usd();
        if remaining <= 0.0 {
            return Ok(Flow::Stop(
                RunStatus::BudgetExhausted,
                format!("local dispatch budget exhausted before the {strategy} runner stage"),
            ));
        }
        self.say(Some(strategy), "running the benchmark stage");
        let request = StageRequest {
            run_id: self.run_id.clone(),
            strategy,
            tool_kb_path: self.output.join(&kb_rel),
            server_version: active.server_version.clone(),
            source_version: self.config.versions.source.clone(),
            task_ids: self.config.tasks.ids.clone(),
            provider: self.config.provider.kind.name().to_owned(),
            api_key_env: self.config.provider.api_key_env.clone(),
            eval_data_env: self.config.inputs.eval_env.clone(),
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature,
            top_p: self.config.provider.top_p,
            max_output_tokens: self.config.provider.max_output_tokens,
            seed: self.config.run.seed,
            judge_version: self.config.judge.version.clone(),
            output_jsonl: self.output.join(format!(
                "python/{}-rep{repetition}.jsonl",
                strategy.as_str()
            )),
            remaining_budget_usd: remaining,
            rate_card: self.config.budget.rate_card.clone(),
            mock: self.config.provider.kind == ConfigProviderKind::Mock,
        };

        let mut sink_error: Option<HarnessError> = None;
        let mut ingest_problems: Vec<String> = Vec::new();
        let result = {
            let emitter = &mut self.emitter;
            let ledger = &mut self.ledger;
            let py_seq = &mut self.py_seq;
            let entry = &mut *entry;
            let mut on_event = |mut event: RunEvent| {
                if sink_error.is_some() {
                    return;
                }
                event.seq = *py_seq;
                match emitter.forward(&event) {
                    Ok(written) => {
                        *py_seq += 1;
                        if let EventBody::ModelCall(call) = &written.body
                            && let Err(e) = ledger.ingest_external(call)
                        {
                            ingest_problems.push(e.to_string());
                        }
                        entry.absorb(&written);
                    }
                    Err(e) => sink_error = Some(e.into()),
                }
            };
            self.runner.run_stage(&request, &mut on_event)
        };
        if let Some(e) = sink_error {
            return Err(e);
        }
        for problem in ingest_problems {
            let message = format!("runner cost record rejected: {problem}");
            self.error_event(strategy, message.clone(), false, entry)?;
            self.fail(message);
        }

        match result {
            Ok(StageOutcome {
                budget_exhausted, ..
            }) => {
                entry.stage = if budget_exhausted {
                    StageStatus::BudgetExhausted
                } else {
                    StageStatus::Completed
                };
                if budget_exhausted {
                    return Ok(Flow::Stop(
                        RunStatus::BudgetExhausted,
                        format!("the runner reported budget exhausted during {strategy}"),
                    ));
                }
                Ok(Flow::Continue)
            }
            Err(RunnerError::Cancelled { .. }) => {
                entry.stage = StageStatus::Cancelled;
                Ok(cancel_flow())
            }
            Err(e) => {
                entry.stage = StageStatus::Failed;
                let message = format!("runner stage failed for {strategy}: {e}");
                self.error_event(strategy, message.clone(), false, entry)?;
                self.fail(message);
                Ok(Flow::Continue)
            }
        }
    }
}

fn cancel_flow() -> Flow {
    Flow::Stop(RunStatus::Cancelled, "cancelled by the operator".to_owned())
}

struct FailedAdaptation {
    status: AdaptationStatus,
    message: String,
    /// The local dispatch cap refused the call: stop the whole matrix.
    budget_stop: bool,
    /// The failure is a harness or provider failure, not a rejected model output.
    harness_failure: bool,
}

enum Adapted {
    Candidate(Candidate),
    Failed(FailedAdaptation),
    Fatal(HarnessError),
}
