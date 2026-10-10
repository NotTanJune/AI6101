//! Canary gate: probes a freshly adapted Tool KB candidate against the TARGET server version
//! before it may replace the prior KB.
//!
//! The canary is distinct from the completion verifier. The verifier judges whether a benchmark
//! task was completed; the canary judges whether the recent adaptation is consistent with the
//! target server version, using one to three small targeted probes (built from the CANDIDATE
//! card's own schema) plus an optional control probe on an unchanged tool to detect collateral
//! damage.
//!
//! Safety rules enforced here:
//! - A probe is never executed unless the executor reports [`ProbeExecutor::is_isolated`]
//!   (disposable, resettable state). A non-isolated executor executes nothing, so a mutating
//!   probe can never touch non-disposable state. Skipped probes are recorded as inconclusive.
//! - Only a fully passing canary permits promotion. Fail and inconclusive both block: the prior
//!   KB is kept and the candidate is never presented as a successful update.
//! - No-update runs have no candidate; [`CanaryGate::not_applicable`] records `not_applicable`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::config::Strategy;
use crate::error::Result;
use crate::events::{
    AdaptationStatus, CanaryResultEvent, CanaryStatus, ChangeKind, ChangeRecord, EventBody,
    EventEmitter, McpActionEvent,
};
use crate::model::{ChangeClass, ToolCard, ToolKb};

/// Maximum number of targeted probes per candidate.
pub const MAX_TARGETED_PROBES: usize = 3;

// ---------------------------------------------------------------------------------------------
// Probes and executors
// ---------------------------------------------------------------------------------------------

/// One small canary probe: a single tool call built from the candidate card.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// Unique probe id.
    pub probe_id: String,
    /// Source-native tool id to call.
    pub tool_id: String,
    /// Lineage id of the tool.
    pub lineage_id: String,
    /// Arguments of the call.
    pub arguments: Map<String, Value>,
    /// Expected observation, as text.
    pub expected: String,
    /// Whether the probe targets an unchanged control tool.
    pub is_control: bool,
    /// Whether the call may change server state.
    pub mutating: bool,
}

/// What an executor observed for one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeObservation {
    /// The expectation was met.
    pub ok: bool,
    /// Actual observation, as text.
    pub actual: String,
    /// The executor could not decide (takes precedence over `ok`).
    pub inconclusive: bool,
}

impl ProbeObservation {
    /// Expectation met.
    pub fn pass(actual: impl Into<String>) -> Self {
        Self {
            ok: true,
            actual: actual.into(),
            inconclusive: false,
        }
    }

    /// Expectation not met.
    pub fn fail(actual: impl Into<String>) -> Self {
        Self {
            ok: false,
            actual: actual.into(),
            inconclusive: false,
        }
    }

    /// Could not decide.
    pub fn inconclusive(actual: impl Into<String>) -> Self {
        Self {
            ok: false,
            actual: actual.into(),
            inconclusive: true,
        }
    }
}

/// Failure inside a probe executor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// The state could not be reset to a known-good baseline.
    #[error("reset failed: {0}")]
    Reset(String),
    /// The probe could not be executed.
    #[error("execution failed: {0}")]
    Execution(String),
}

/// Runs probes against the target server version.
pub trait ProbeExecutor {
    /// Whether all state touched by probes is disposable and resettable. Probes are only ever
    /// executed when this is `true`.
    fn is_isolated(&self) -> bool;
    /// Restore the baseline state. Called before every probe.
    fn reset(&mut self) -> std::result::Result<(), ProbeError>;
    /// Execute one probe.
    fn execute(&mut self, probe: &Probe) -> std::result::Result<ProbeObservation, ProbeError>;
}

/// In-process executor that checks probe arguments against the TARGET version's card.
///
/// A probe passes when its tool exists in the target (by `tool_id`) and its arguments satisfy
/// the target `input_schema`: every required property is present, every supplied property is
/// declared (unless `additionalProperties` is true or a schema) and has the declared type,
/// and enum/minimum/maximum constraints hold. Unknown tool means fail.
#[derive(Debug, Clone)]
pub struct SimulatedTargetExecutor {
    tools: BTreeMap<String, ToolCard>,
}

impl SimulatedTargetExecutor {
    /// Build from the target version's KB.
    pub fn from_target_kb(target: &ToolKb) -> Self {
        Self {
            tools: target
                .cards
                .iter()
                .map(|c| (c.tool_id.clone(), c.clone()))
                .collect(),
        }
    }
}

impl ProbeExecutor for SimulatedTargetExecutor {
    fn is_isolated(&self) -> bool {
        true
    }

    fn reset(&mut self) -> std::result::Result<(), ProbeError> {
        Ok(())
    }

    fn execute(&mut self, probe: &Probe) -> std::result::Result<ProbeObservation, ProbeError> {
        let Some(card) = self.tools.get(&probe.tool_id) else {
            return Ok(ProbeObservation::fail(format!(
                "target has no tool '{}'",
                probe.tool_id
            )));
        };
        Ok(
            match validate_arguments(&card.input_schema, &probe.arguments) {
                Ok(()) => ProbeObservation::pass(format!(
                    "accepted: arguments valid against target schema of '{}'",
                    probe.tool_id
                )),
                Err(message) => ProbeObservation::fail(format!("rejected by target: {message}")),
            },
        )
    }
}

/// Executor for tests: canned per-probe outcomes and a record of what was executed.
#[derive(Debug, Clone)]
pub struct ScriptedExecutor {
    isolated: bool,
    outcomes: BTreeMap<String, std::result::Result<ProbeObservation, ProbeError>>,
    default_outcome: ProbeObservation,
    reset_error: Option<String>,
    /// Probe ids passed to `execute`, in order.
    pub executed: Vec<String>,
    /// Number of `reset` calls.
    pub resets: usize,
}

impl ScriptedExecutor {
    /// Executor whose probes pass unless scripted otherwise.
    pub fn new(isolated: bool) -> Self {
        Self {
            isolated,
            outcomes: BTreeMap::new(),
            default_outcome: ProbeObservation::pass("scripted pass"),
            reset_error: None,
            executed: Vec::new(),
            resets: 0,
        }
    }

    /// Script the outcome of one probe.
    #[must_use]
    pub fn with_outcome(
        mut self,
        probe_id: impl Into<String>,
        outcome: std::result::Result<ProbeObservation, ProbeError>,
    ) -> Self {
        self.outcomes.insert(probe_id.into(), outcome);
        self
    }

    /// Outcome for probes without a scripted one.
    #[must_use]
    pub fn with_default(mut self, outcome: ProbeObservation) -> Self {
        self.default_outcome = outcome;
        self
    }

    /// Make every `reset` fail with `message`.
    #[must_use]
    pub fn with_reset_error(mut self, message: impl Into<String>) -> Self {
        self.reset_error = Some(message.into());
        self
    }
}

impl ProbeExecutor for ScriptedExecutor {
    fn is_isolated(&self) -> bool {
        self.isolated
    }

    fn reset(&mut self) -> std::result::Result<(), ProbeError> {
        self.resets += 1;
        match &self.reset_error {
            Some(message) => Err(ProbeError::Reset(message.clone())),
            None => Ok(()),
        }
    }

    fn execute(&mut self, probe: &Probe) -> std::result::Result<ProbeObservation, ProbeError> {
        self.executed.push(probe.probe_id.clone());
        self.outcomes
            .get(&probe.probe_id)
            .cloned()
            .unwrap_or_else(|| Ok(self.default_outcome.clone()))
    }
}

// ---------------------------------------------------------------------------------------------
// Argument validation (simulated target)
// ---------------------------------------------------------------------------------------------

fn validate_arguments(
    schema: &Value,
    arguments: &Map<String, Value>,
) -> std::result::Result<(), String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    for name in required_names(schema) {
        if !arguments.contains_key(name) {
            return Err(format!("missing required argument '{name}'"));
        }
    }
    let open = matches!(
        schema.get("additionalProperties"),
        Some(Value::Bool(true) | Value::Object(_))
    );
    for (name, value) in arguments {
        match properties.and_then(|p| p.get(name)) {
            Some(prop_schema) => check_value(prop_schema, value, name)?,
            None if properties.is_none() || open => {}
            None => return Err(format!("unknown argument '{name}'")),
        }
    }
    Ok(())
}

fn required_names(schema: &Value) -> impl Iterator<Item = &str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn type_matches(type_name: &str, value: &Value) -> bool {
    match type_name {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value
                    .as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        // Unknown type keyword: do not reject on it.
        _ => true,
    }
}

fn check_value(schema: &Value, value: &Value, path: &str) -> std::result::Result<(), String> {
    match schema.get("type") {
        Some(Value::String(t)) if !type_matches(t, value) => {
            return Err(format!("argument '{path}' must be of type {t}"));
        }
        Some(Value::Array(types))
            if !types
                .iter()
                .filter_map(Value::as_str)
                .any(|t| type_matches(t, value)) =>
        {
            return Err(format!(
                "argument '{path}' has a type outside the declared types"
            ));
        }
        _ => {}
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!(
            "argument '{path}' is not one of the allowed enum values"
        ));
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("argument '{path}' is below the minimum {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("argument '{path}' is above the maximum {max}"));
        }
    }
    if let Some(object) = value.as_object() {
        for name in required_names(schema) {
            if !object.contains_key(name) {
                return Err(format!(
                    "argument '{path}' is missing required field '{name}'"
                ));
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, inner) in object {
                if let Some(prop_schema) = props.get(name) {
                    check_value(prop_schema, inner, &format!("{path}.{name}"))?;
                }
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            check_value(item_schema, item, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Probe generation
// ---------------------------------------------------------------------------------------------

const MAX_SAMPLE_DEPTH: usize = 4;

/// Type-appropriate sample for a schema: const, first enum value, default, first example, then
/// a plain default of the declared type.
fn sample_value(schema: &Value, depth: usize) -> Value {
    if let Some(v) = schema.get("const") {
        return v.clone();
    }
    if let Some(v) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return v.clone();
    }
    if let Some(v) = schema.get("default") {
        return v.clone();
    }
    if let Some(v) = schema
        .get("examples")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return v.clone();
    }
    let declared = match schema.get("type") {
        Some(Value::String(t)) => Some(t.as_str()),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .or(Some("null")),
        _ => None,
    };
    match declared {
        Some("integer") => schema
            .get("minimum")
            .and_then(Value::as_f64)
            .map_or(Value::from(1), |min| Value::from(min.ceil() as i64)),
        Some("number") => schema
            .get("minimum")
            .and_then(Value::as_f64)
            .map_or(Value::from(1.0), Value::from),
        Some("boolean") => Value::Bool(false),
        Some("null") => Value::Null,
        Some("array") => {
            let wants_item = schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0);
            match (wants_item, schema.get("items")) {
                (true, Some(items)) if depth < MAX_SAMPLE_DEPTH => {
                    Value::Array(vec![sample_value(items, depth + 1)])
                }
                _ => Value::Array(Vec::new()),
            }
        }
        Some("object") => Value::Object(minimal_arguments_at(schema, depth + 1)),
        Some(_) => Value::String("sample".to_owned()),
        None => match schema
            .get("oneOf")
            .or_else(|| schema.get("anyOf"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            Some(first) if depth < MAX_SAMPLE_DEPTH => sample_value(first, depth + 1),
            _ => Value::String("sample".to_owned()),
        },
    }
}

fn minimal_arguments_at(schema: &Value, depth: usize) -> Map<String, Value> {
    let mut out = Map::new();
    if depth > MAX_SAMPLE_DEPTH {
        return out;
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for name in required_names(schema) {
        let value = match properties.and_then(|p| p.get(name)) {
            Some(prop_schema) => sample_value(prop_schema, depth),
            None => Value::String("sample".to_owned()),
        };
        out.insert(name.to_owned(), value);
    }
    out
}

/// Minimal valid arguments for `schema`: required properties only, with type-appropriate
/// values taken from `const`/`enum`/`default`/`examples`, else a plain default of the type.
pub fn minimal_arguments(schema: &Value) -> Map<String, Value> {
    minimal_arguments_at(schema, 0)
}

const MUTATING_VERBS: &[&str] = &[
    "add", "approve", "cancel", "clear", "commit", "create", "delete", "drop", "edit", "execute",
    "insert", "kill", "modify", "move", "patch", "post", "publish", "push", "put", "remove",
    "rename", "reset", "run", "send", "set", "update", "upload", "write",
];

/// Whether a card's name suggests a state-changing tool. Conservative: any name token equal
/// to a mutating verb counts.
pub fn looks_mutating(card: &ToolCard) -> bool {
    [card.tool_id.as_str(), card.name.as_str()]
        .into_iter()
        .flat_map(name_tokens)
        .any(|t| MUTATING_VERBS.contains(&t.as_str()))
}

fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current.extend(ch.to_lowercase());
        prev_lower = ch.is_lowercase();
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn change_rank(record: &ChangeRecord) -> u8 {
    match (record.class, record.kind) {
        (ChangeClass::Parameter, _) => 0,
        (ChangeClass::Tool, _) => 1,
        (ChangeClass::Description, _) => 2,
    }
}

fn find_card<'a>(kb: &'a ToolKb, lineage_id: &str, tool_id: &str) -> Option<&'a ToolCard> {
    let mut by_lineage = kb.cards.iter().filter(|c| c.lineage_id == lineage_id);
    let first = by_lineage.next()?;
    if first.tool_id == tool_id {
        return Some(first);
    }
    Some(by_lineage.find(|c| c.tool_id == tool_id).unwrap_or(first))
}

/// Pick a control card: the first card (by `(server_id, lineage_id)`) whose lineage is not in
/// `changed` and, when `target` is given, still exists in the target version.
pub fn pick_control<'a>(
    candidate: &'a ToolKb,
    changed: &[ChangeRecord],
    target: Option<&ToolKb>,
) -> Option<&'a ToolCard> {
    let changed_lineages: BTreeSet<&str> = changed.iter().map(|c| c.lineage_id.as_str()).collect();
    let mut cards: Vec<&ToolCard> = candidate.cards.iter().collect();
    cards.sort_by(|a, b| (&a.server_id, &a.lineage_id).cmp(&(&b.server_id, &b.lineage_id)));
    cards.into_iter().find(|c| {
        !changed_lineages.contains(c.lineage_id.as_str())
            && target.is_none_or(|t| t.cards.iter().any(|tc| tc.lineage_id == c.lineage_id))
    })
}

/// Build the canary probes from the CANDIDATE KB.
///
/// One to three targeted probes cover the changed tools present in the candidate (removed tools
/// are skipped; parameter changes come first, then tool changes, then description changes;
/// ties by lineage id). Arguments are minimal valid arguments from the candidate card's
/// `input_schema`. When `control` is given, is not itself a changed tool, and at least one
/// targeted probe exists, one control probe from that unchanged card is appended.
pub fn generate_probes(
    candidate: &ToolKb,
    changed: &[ChangeRecord],
    control: Option<&ToolCard>,
) -> Vec<Probe> {
    let mut ranked: BTreeMap<&str, (u8, &ChangeRecord)> = BTreeMap::new();
    for record in changed.iter().filter(|r| r.kind != ChangeKind::Removed) {
        let rank = change_rank(record);
        ranked
            .entry(record.lineage_id.as_str())
            .and_modify(|existing| {
                if rank < existing.0 {
                    *existing = (rank, record);
                }
            })
            .or_insert((rank, record));
    }
    let mut order: Vec<(u8, &str, &ChangeRecord)> = ranked
        .into_iter()
        .map(|(lineage, (rank, record))| (rank, lineage, record))
        .collect();
    order.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));

    let mut probes = Vec::new();
    for (_, lineage, record) in order {
        if probes.len() >= MAX_TARGETED_PROBES {
            break;
        }
        if let Some(card) = find_card(candidate, lineage, &record.tool_id) {
            probes.push(build_probe(
                format!("canary-{}-{lineage}", probes.len() + 1),
                card,
                false,
            ));
        }
    }
    if !probes.is_empty()
        && let Some(card) = control
        && !changed.iter().any(|c| c.lineage_id == card.lineage_id)
    {
        probes.push(build_probe(
            format!("canary-control-{}", card.lineage_id),
            card,
            true,
        ));
    }
    probes
}

fn build_probe(probe_id: String, card: &ToolCard, is_control: bool) -> Probe {
    let role = if is_control {
        "unchanged control tool"
    } else {
        "changed tool"
    };
    Probe {
        probe_id,
        tool_id: card.tool_id.clone(),
        lineage_id: card.lineage_id.clone(),
        arguments: minimal_arguments(&card.input_schema),
        expected: format!(
            "target server version accepts a call to {role} '{}' built from the candidate card",
            card.tool_id
        ),
        is_control,
        mutating: looks_mutating(card),
    }
}

// ---------------------------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------------------------

/// Outcome of one probe (or of the gate itself when no probe could be built).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    /// Probe id (`None` for the synthetic row recorded when no probe could be built).
    pub probe_id: Option<String>,
    /// Tool probed.
    pub tool_id: Option<String>,
    /// Whether this was the control probe.
    pub is_control: bool,
    /// `pass`, `fail` or `inconclusive`.
    pub status: CanaryStatus,
    /// Expected observation.
    pub expected: Option<String>,
    /// Actual observation, or the reason the probe was not executed.
    pub actual: Option<String>,
    /// Whether the probe was actually handed to the executor.
    pub executed: bool,
    /// Sequence number of the `mcp_action` event (present only when a call was attempted).
    pub action_seq: Option<u64>,
    /// Sequence number of the `canary_result` event.
    pub result_seq: u64,
}

/// Aggregate canary result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryReport {
    /// `pass` only if every targeted probe and the control passed; `fail` if any failed;
    /// otherwise `inconclusive`. `not_applicable` for no-update runs.
    pub status: CanaryStatus,
    /// One entry per probe, in execution order.
    pub results: Vec<ProbeResult>,
    /// The candidate may be promoted. Always `status == Pass`.
    pub promote: bool,
    /// A control probe ran and failed: the candidate damaged an unchanged tool.
    pub collateral_damage: bool,
    /// A control probe was part of the run.
    pub control_included: bool,
}

impl CanaryReport {
    /// Human-readable reason, suitable for an `adaptation_outcome` event.
    pub fn summary(&self) -> String {
        match self.status {
            CanaryStatus::Pass => format!("canary passed ({} probe(s))", self.results.len()),
            CanaryStatus::NotApplicable => {
                "canary not applicable: no candidate to probe".to_owned()
            }
            CanaryStatus::Fail | CanaryStatus::Inconclusive => {
                let label = if self.status == CanaryStatus::Fail {
                    "canary failed"
                } else {
                    "canary inconclusive"
                };
                let details: Vec<String> = self
                    .results
                    .iter()
                    .filter(|r| r.status != CanaryStatus::Pass)
                    .map(|r| {
                        format!(
                            "{}{} [{}]: {}",
                            if r.is_control { "control " } else { "" },
                            r.probe_id.as_deref().unwrap_or("(no probe)"),
                            status_word(r.status),
                            r.actual.as_deref().unwrap_or("no observation")
                        )
                    })
                    .collect();
                let mut text = format!("{label}: {}", details.join("; "));
                if self.collateral_damage {
                    text.push_str(" (collateral damage on unchanged tool)");
                }
                text
            }
        }
    }
}

fn status_word(status: CanaryStatus) -> &'static str {
    match status {
        CanaryStatus::Pass => "pass",
        CanaryStatus::Fail => "fail",
        CanaryStatus::Inconclusive => "inconclusive",
        CanaryStatus::NotApplicable => "not_applicable",
    }
}

/// The canary gate. Stateless: all methods are associated functions.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanaryGate;

impl CanaryGate {
    /// Generate probes from `candidate`, run them on `executor` and record every step.
    ///
    /// `target`, when given, restricts the control probe to tools that still exist in the
    /// target version. For each executed probe an `mcp_action` event (source rust) is emitted
    /// first and then a `canary_result` whose `event_refs` hold that action's `seq`. A probe
    /// that is not executed emits only its `canary_result` (inconclusive, reason in `actual`,
    /// empty `event_refs`).
    pub fn run(
        candidate: &ToolKb,
        changed: &[ChangeRecord],
        target: Option<&ToolKb>,
        executor: &mut dyn ProbeExecutor,
        emitter: &mut EventEmitter,
        strategy: Strategy,
    ) -> Result<CanaryReport> {
        let control = pick_control(candidate, changed, target);
        let probes = generate_probes(candidate, changed, control);
        Self::run_probes(&probes, executor, emitter, strategy)
    }

    /// Run prebuilt probes (see [`generate_probes`]).
    pub fn run_probes(
        probes: &[Probe],
        executor: &mut dyn ProbeExecutor,
        emitter: &mut EventEmitter,
        strategy: Strategy,
    ) -> Result<CanaryReport> {
        let mut results = Vec::new();
        if probes.iter().all(|p| p.is_control) {
            let expected = "at least one targeted probe for a changed tool".to_owned();
            let actual =
                "no targeted probe could be built for the changed tools; nothing to verify"
                    .to_owned();
            let result_event = emitter.emit(
                Some(strategy),
                EventBody::CanaryResult(CanaryResultEvent {
                    status: CanaryStatus::Inconclusive,
                    tool_id: None,
                    probe_id: None,
                    is_control: false,
                    expected: Some(expected.clone()),
                    actual: Some(actual.clone()),
                    event_refs: Vec::new(),
                }),
            )?;
            results.push(ProbeResult {
                probe_id: None,
                tool_id: None,
                is_control: false,
                status: CanaryStatus::Inconclusive,
                expected: Some(expected),
                actual: Some(actual),
                executed: false,
                action_seq: None,
                result_seq: result_event.seq,
            });
            return Ok(Self::report(results, !probes.is_empty()));
        }

        for (index, probe) in probes.iter().enumerate() {
            results.push(Self::run_one(index, probe, executor, emitter, strategy)?);
        }
        Ok(Self::report(results, probes.iter().any(|p| p.is_control)))
    }

    fn run_one(
        index: usize,
        probe: &Probe,
        executor: &mut dyn ProbeExecutor,
        emitter: &mut EventEmitter,
        strategy: Strategy,
    ) -> Result<ProbeResult> {
        // Never touch non-disposable state: a non-isolated executor executes nothing.
        let attempt: std::result::Result<ProbeObservation, (bool, String)> =
            if !executor.is_isolated() {
                Err((
                    false,
                    format!(
                        "not executed: executor is not isolated (disposable, resettable state){}",
                        if probe.mutating {
                            "; probe is mutating"
                        } else {
                            ""
                        }
                    ),
                ))
            } else if let Err(e) = executor.reset() {
                Err((false, format!("not executed: {e}")))
            } else {
                executor
                    .execute(probe)
                    .map_err(|e| (true, format!("executor error: {e}")))
            };

        let (executed, status, actual, action_seq) = match attempt {
            Ok(observation) => {
                let status = if observation.inconclusive {
                    CanaryStatus::Inconclusive
                } else if observation.ok {
                    CanaryStatus::Pass
                } else {
                    CanaryStatus::Fail
                };
                let action = Self::emit_action(
                    index,
                    probe,
                    &observation.actual,
                    status != CanaryStatus::Pass,
                    emitter,
                    strategy,
                )?;
                (true, status, observation.actual, Some(action))
            }
            Err((true, message)) => {
                let action = Self::emit_action(index, probe, &message, true, emitter, strategy)?;
                (true, CanaryStatus::Inconclusive, message, Some(action))
            }
            Err((false, message)) => (false, CanaryStatus::Inconclusive, message, None),
        };

        let result_event = emitter.emit(
            Some(strategy),
            EventBody::CanaryResult(CanaryResultEvent {
                status,
                tool_id: Some(probe.tool_id.clone()),
                probe_id: Some(probe.probe_id.clone()),
                is_control: probe.is_control,
                expected: Some(probe.expected.clone()),
                actual: Some(actual.clone()),
                event_refs: action_seq.into_iter().collect(),
            }),
        )?;
        Ok(ProbeResult {
            probe_id: Some(probe.probe_id.clone()),
            tool_id: Some(probe.tool_id.clone()),
            is_control: probe.is_control,
            status,
            expected: Some(probe.expected.clone()),
            actual: Some(actual),
            executed,
            action_seq,
            result_seq: result_event.seq,
        })
    }

    fn emit_action(
        index: usize,
        probe: &Probe,
        summary: &str,
        is_error: bool,
        emitter: &mut EventEmitter,
        strategy: Strategy,
    ) -> Result<u64> {
        let event = emitter.emit(
            Some(strategy),
            EventBody::McpAction(McpActionEvent {
                task_id: format!("canary/{}", probe.probe_id),
                step: u32::try_from(index).unwrap_or(u32::MAX),
                tool_id: probe.tool_id.clone(),
                arguments: probe.arguments.clone(),
                result_summary: summary.to_owned(),
                is_error,
            }),
        )?;
        Ok(event.seq)
    }

    fn report(results: Vec<ProbeResult>, control_included: bool) -> CanaryReport {
        let any_fail = results.iter().any(|r| r.status == CanaryStatus::Fail);
        let collateral_damage = results
            .iter()
            .any(|r| r.is_control && r.status == CanaryStatus::Fail);
        let has_targeted = results
            .iter()
            .any(|r| !r.is_control && r.probe_id.is_some());
        let all_pass =
            !results.is_empty() && results.iter().all(|r| r.status == CanaryStatus::Pass);
        let status = if any_fail {
            CanaryStatus::Fail
        } else if has_targeted && all_pass {
            CanaryStatus::Pass
        } else {
            CanaryStatus::Inconclusive
        };
        CanaryReport {
            status,
            promote: status == CanaryStatus::Pass,
            results,
            collateral_damage,
            control_included,
        }
    }

    /// Record that the canary does not apply (no-update runs have no candidate).
    pub fn not_applicable(emitter: &mut EventEmitter, strategy: Strategy) -> Result<CanaryReport> {
        let reason = "no candidate to probe (no update performed)".to_owned();
        let event = emitter.emit(
            Some(strategy),
            EventBody::CanaryResult(CanaryResultEvent {
                status: CanaryStatus::NotApplicable,
                tool_id: None,
                probe_id: None,
                is_control: false,
                expected: None,
                actual: Some(reason.clone()),
                event_refs: Vec::new(),
            }),
        )?;
        Ok(CanaryReport {
            status: CanaryStatus::NotApplicable,
            results: vec![ProbeResult {
                probe_id: None,
                tool_id: None,
                is_control: false,
                status: CanaryStatus::NotApplicable,
                expected: None,
                actual: Some(reason),
                executed: false,
                action_seq: None,
                result_seq: event.seq,
            }],
            promote: false,
            collateral_damage: false,
            control_included: false,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Promotion decision
// ---------------------------------------------------------------------------------------------

/// What to do with the candidate after the canary.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionDecision {
    /// Canary passed: the candidate KB becomes the active KB.
    Promote(ToolKb),
    /// Canary failed or was inconclusive: the prior KB is kept, the candidate is blocked and
    /// must never be presented as a successful update.
    Blocked {
        /// Why the candidate was blocked.
        reason: String,
        /// The KB to keep using (the prior KB).
        kept: ToolKb,
    },
    /// No candidate existed (canary `not_applicable`): the prior KB stays unchanged.
    Unchanged {
        /// The unchanged prior KB.
        kept: ToolKb,
    },
}

impl PromotionDecision {
    /// The KB that is active after the decision.
    pub fn active_kb(&self) -> &ToolKb {
        match self {
            Self::Promote(kb) => kb,
            Self::Blocked { kept, .. } | Self::Unchanged { kept } => kept,
        }
    }

    /// Matching `adaptation_outcome` status.
    pub fn adaptation_status(&self) -> AdaptationStatus {
        match self {
            Self::Promote(_) => AdaptationStatus::Promoted,
            Self::Blocked { .. } => AdaptationStatus::Blocked,
            Self::Unchanged { .. } => AdaptationStatus::Unchanged,
        }
    }
}

/// Decide whether `candidate` replaces `prior`.
///
/// Promote only when the report status is `pass` (and its `promote` flag agrees). Fail and
/// inconclusive block and keep `prior`. A `not_applicable` report means there was nothing to
/// promote: `prior` is kept as [`PromotionDecision::Unchanged`].
pub fn decide_promotion(
    report: &CanaryReport,
    prior: &ToolKb,
    candidate: &ToolKb,
) -> PromotionDecision {
    match report.status {
        CanaryStatus::Pass if report.promote => PromotionDecision::Promote(candidate.clone()),
        CanaryStatus::NotApplicable => PromotionDecision::Unchanged {
            kept: prior.clone(),
        },
        _ => PromotionDecision::Blocked {
            reason: report.summary(),
            kept: prior.clone(),
        },
    }
}
