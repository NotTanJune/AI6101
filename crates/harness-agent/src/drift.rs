//! Drift between the Tool KB and a live MCP server, and the adaptation flow built on it.
//!
//! Detection converts the live `tools/list` into a temporary Tool KB and diffs it against the
//! session KB with core's `diff_kbs`. An adaptation strategy can then propose a candidate KB
//! (checked by a canary against the live definitions), but the session KB changes only when the
//! caller explicitly calls [`SessionKb::accept`]. Nothing is accepted automatically.

use std::sync::Arc;

use harness_core::HarnessError;
use harness_core::canary::{
    CanaryGate, CanaryReport, Probe, ProbeError, ProbeExecutor, ProbeObservation,
    PromotionDecision, decide_promotion,
};
use harness_core::clock::SystemClock;
use harness_core::config::Strategy;
use harness_core::diff::{KbDiff, diff_kbs};
use harness_core::events::{EventEmitter, EventSource, VecSink};
use harness_core::model::{Provenance, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb};
use harness_core::provider::MeteredProvider;
use harness_core::redact::Redactor;
use harness_core::strategy::{
    AdaptationContext, Candidate, StrategyError, strategy_for, validate_candidate_against_source,
};

use crate::mcp::LiveTool;

/// Version label of the temporary KB built from a live server.
pub const LIVE_VERSION: &str = "live";

/// Difference between the KB and one live server.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftReport {
    /// Server name.
    pub server: String,
    /// KB cards of that server.
    pub kb_cards: usize,
    /// Tools the live server lists.
    pub live_tools: usize,
    /// Changes from the KB (source) to the live definitions (target).
    pub diff: KbDiff,
}

impl DriftReport {
    /// Whether the live server matches the KB.
    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }
}

/// The KB restricted to the cards of one server.
pub fn kb_subset(kb: &ToolKb, server: &str) -> ToolKb {
    ToolKb {
        cards: kb
            .cards
            .iter()
            .filter(|c| c.server_id == server)
            .cloned()
            .collect(),
        ..kb.clone()
    }
}

/// Convert a live `tools/list` into a temporary Tool KB.
///
/// Cards get the server id, `server_version = "live"`, and the KB lineage id when the tool name
/// matches a KB card (`live:<name>` otherwise). A live listing carries no usage rules, so a
/// matching KB card's rules are kept; this stops KB-only knowledge from showing up as drift.
pub fn live_kb(server: &str, kb: &ToolKb, live: &[LiveTool]) -> ToolKb {
    let cards =
        live.iter()
            .map(|tool| {
                let matched: Option<&ToolCard> = kb.cards.iter().find(|c| {
                    c.server_id == server && (c.tool_id == tool.name || c.name == tool.name)
                });
                ToolCard {
                    lineage_id: matched
                        .map_or_else(|| format!("live:{}", tool.name), |c| c.lineage_id.clone()),
                    server_id: server.to_owned(),
                    tool_id: tool.name.clone(),
                    name: tool.name.clone(),
                    server_version: LIVE_VERSION.to_owned(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                    output_schema: tool.output_schema.clone(),
                    usage_rules: matched.map(|c| c.usage_rules.clone()).unwrap_or_default(),
                    provenance: Provenance {
                        release_id: kb.release_id.clone(),
                        source_file: "live tools/list".to_owned(),
                        source_pointer: None,
                    },
                }
            })
            .collect();
    ToolKb {
        schema_version: TOOL_KB_SCHEMA_VERSION.to_owned(),
        kb_id: format!("live:{server}"),
        release_id: kb.release_id.clone(),
        server_version: LIVE_VERSION.to_owned(),
        cards,
    }
}

/// Compare the KB with a live server. Returns the report and the temporary live KB.
pub fn detect_drift(server: &str, kb: &ToolKb, live: &[LiveTool]) -> (DriftReport, ToolKb) {
    let subset = kb_subset(kb, server);
    let live_kb = live_kb(server, kb, live);
    let diff = diff_kbs(&subset, &live_kb);
    (
        DriftReport {
            server: server.to_owned(),
            kb_cards: subset.cards.len(),
            live_tools: live.len(),
            diff,
        },
        live_kb,
    )
}

/// Drift flow failure.
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    /// The strategy failed or its output was rejected.
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    /// The canary could not run.
    #[error("canary failed to run: {0}")]
    Canary(String),
}

/// A proposed KB update and how the canary judged it.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptationOutcome {
    /// Strategy used.
    pub strategy: Strategy,
    /// The candidate KB and its provenance.
    pub candidate: Candidate,
    /// Canary report (`not_applicable` for `NoUpdate`).
    pub canary: CanaryReport,
    /// What core's promotion rule says to do with the candidate.
    pub decision: PromotionDecision,
}

/// Prepare and validate a strategy's candidate against the source and live definitions.
///
/// `kb_subset` should come from [`kb_subset`], and `live_kb` and `diff` from [`detect_drift`].
/// This performs no probes and changes no session state.
pub fn prepare_candidate(
    strategy: Strategy,
    kb_subset: &ToolKb,
    live_kb: &ToolKb,
    diff: &KbDiff,
    provider: &mut MeteredProvider<'_>,
) -> Result<Candidate, DriftError> {
    let strat = strategy_for(strategy);
    let candidate = {
        let mut ctx = AdaptationContext {
            source: kb_subset,
            target: live_kb,
            diff,
            provider,
        };
        strat.adapt(&mut ctx)?
    };
    validate_candidate_against_source(kb_subset, &candidate.kb, live_kb, diff, strategy)?;
    Ok(candidate)
}

/// Evaluate a prepared candidate with the caller's executor and durable event emitter.
///
/// Use [`prepare_candidate`] first. For live servers use
/// [`LiveCanaryExecutor`](crate::live_canary::LiveCanaryExecutor) inside a blocking task.
/// The outcome remains a proposal until the caller explicitly accepts it.
pub fn evaluate_candidate(
    strategy: Strategy,
    kb_subset: &ToolKb,
    live_kb: &ToolKb,
    candidate: Candidate,
    executor: &mut dyn ProbeExecutor,
    emitter: &mut EventEmitter,
) -> Result<AdaptationOutcome, DriftError> {
    let canary = if strategy == Strategy::NoUpdate {
        CanaryGate::not_applicable(emitter, strategy)
    } else {
        CanaryGate::run(
            &candidate.kb,
            &candidate.changes,
            Some(live_kb),
            executor,
            emitter,
            strategy,
        )
    }
    .map_err(|e: HarnessError| DriftError::Canary(e.to_string()))?;
    let decision = decide_promotion(&canary, kb_subset, &candidate.kb);
    Ok(AdaptationOutcome {
        strategy,
        candidate,
        canary,
        decision,
    })
}

/// Prepare a candidate without permission to execute any live probes.
///
/// This compatibility entry point fails closed: updating strategies receive an
/// inconclusive canary and cannot be promoted. Supply a live executor and persistent
/// emitter through [`prepare_candidate`] and [`evaluate_candidate`] to run a live check.
pub fn run_adaptation(
    strategy: Strategy,
    kb_subset: &ToolKb,
    live_kb: &ToolKb,
    diff: &KbDiff,
    provider: &mut MeteredProvider<'_>,
) -> Result<AdaptationOutcome, DriftError> {
    let candidate = prepare_candidate(strategy, kb_subset, live_kb, diff, provider)?;
    let mut emitter = EventEmitter::new(
        "drift-canary",
        EventSource::Rust,
        Arc::new(SystemClock),
        Redactor::default(),
        Box::new(VecSink::new()),
    );
    evaluate_candidate(
        strategy,
        kb_subset,
        live_kb,
        candidate,
        &mut NoLivePermission,
        &mut emitter,
    )
}

struct NoLivePermission;

impl ProbeExecutor for NoLivePermission {
    fn is_isolated(&self) -> bool {
        false
    }

    fn reset(&mut self) -> Result<(), ProbeError> {
        Err(ProbeError::Reset(
            "live canary permission not supplied".into(),
        ))
    }

    fn execute(&mut self, _probe: &Probe) -> Result<ProbeObservation, ProbeError> {
        Err(ProbeError::Execution(
            "live canary permission not supplied".into(),
        ))
    }
}

/// The KB a session works from. It changes only through an explicit [`accept`](Self::accept).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionKb {
    kb: ToolKb,
}

impl SessionKb {
    /// Start from a KB.
    pub fn new(kb: ToolKb) -> Self {
        Self { kb }
    }

    /// The current KB.
    pub fn kb(&self) -> &ToolKb {
        &self.kb
    }

    /// Replace the cards of the candidate's servers with the candidate's cards. Cards of other
    /// servers are kept. Returns the number of cards that changed.
    pub fn accept(&mut self, candidate: &Candidate) -> usize {
        let servers: std::collections::BTreeSet<&str> = candidate
            .kb
            .cards
            .iter()
            .map(|c| c.server_id.as_str())
            .collect();
        let before = self.kb.clone();
        let mut cards: Vec<ToolCard> = self
            .kb
            .cards
            .iter()
            .filter(|c| !servers.contains(c.server_id.as_str()))
            .cloned()
            .collect();
        cards.extend(candidate.kb.cards.iter().cloned());
        self.kb.cards = cards;
        self.kb.kb_id.clone_from(&candidate.kb.kb_id);
        self.kb
            .server_version
            .clone_from(&candidate.kb.server_version);
        self.kb
            .cards
            .iter()
            .filter(|c| !before.cards.contains(c))
            .count()
    }
}
