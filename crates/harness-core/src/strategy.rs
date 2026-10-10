//! The three adaptation strategies and candidate validation.
//!
//! * [`NoUpdate`] returns the source KB unchanged and never calls a provider.
//! * [`FullRewrite`] asks the model to regenerate the whole KB from the target definitions.
//! * [`SelectivePatch`] asks the model for a structured [`Patch`] limited to the detected
//!   diff and applies it to the source KB.
//!
//! Model output is never repaired or broadened: it either parses, validates and becomes a
//! [`Candidate`], or the strategy returns a [`StrategyError`]. Prompts are built only from the
//! diff and the target definitions; task text is never used to infer changes.
//!
//! Harness-owned metadata (KB header fields, and `server_version` / `provenance` of cards the
//! candidate updated) is copied from the target definitions after the model answered. The
//! model is neither asked for nor trusted with those values; content fields are never touched.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Strategy;
use crate::diff::KbDiff;
use crate::events::{ChangeKind, ChangeRecord, Phase};
use crate::model::{ChangeClass, ToolCard, ToolKb, canonical_json};
use crate::patch::{
    Patch, PatchError, apply_patch, parse_patch, patch_json_schema, validate_patch,
};
use crate::provider::{MeteredError, MeteredProvider};
use crate::schema::{TOOL_KB_SCHEMA_JSON, validate_tool_kb};

/// Result of a strategy: a proposed KB plus the bookkeeping the pipeline needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Strategy that produced the candidate.
    pub strategy: Strategy,
    /// The candidate KB.
    pub kb: ToolKb,
    /// Hash of the prior (source) KB, kept for rollback.
    pub prior_hash: String,
    /// Hash of the candidate KB.
    pub candidate_hash: String,
    /// Lineage ids the adaptation was allowed or required to change.
    pub changed_lineage_ids: Vec<String>,
    /// Detected changes the candidate responds to.
    pub changes: Vec<ChangeRecord>,
    /// The applied patch (selective patch only).
    pub patch: Option<Patch>,
}

/// Strategy failure. Everything except [`StrategyError::Provider`] means the model output was
/// rejected.
#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    /// The provider call failed or was refused by the budget.
    #[error(transparent)]
    Provider(#[from] MeteredError),
    /// The model output could not be parsed as the requested document.
    #[error("model output rejected: {0}")]
    Output(String),
    /// The patch was invalid or reached outside the detected changes.
    #[error("patch rejected: {0}")]
    Patch(#[from] PatchError),
    /// The candidate does not match the target definitions.
    #[error("candidate rejected: {}", problems.join("; "))]
    Validation {
        /// Every problem found.
        problems: Vec<String>,
    },
}

impl StrategyError {
    /// Whether the failure is a rejection of model output (as opposed to a provider or budget
    /// failure).
    pub fn is_rejection(&self) -> bool {
        !matches!(self, Self::Provider(_))
    }
}

/// Everything a strategy needs.
pub struct AdaptationContext<'a, 'p> {
    /// The KB describing the source version.
    pub source: &'a ToolKb,
    /// The KB (target MCP definitions) describing the target version.
    pub target: &'a ToolKb,
    /// Diff between them.
    pub diff: &'a KbDiff,
    /// Metered access to the model.
    pub provider: &'a mut MeteredProvider<'p>,
}

/// One adaptation strategy.
pub trait AdaptationStrategy {
    /// Which strategy this is.
    fn strategy(&self) -> Strategy;
    /// Produce a validated candidate or an error.
    fn adapt(&self, ctx: &mut AdaptationContext<'_, '_>) -> Result<Candidate, StrategyError>;
}

/// Strategy implementation for `strategy`.
pub fn strategy_for(strategy: Strategy) -> Box<dyn AdaptationStrategy> {
    match strategy {
        Strategy::NoUpdate => Box::new(NoUpdate),
        Strategy::FullRewrite => Box::new(FullRewrite),
        Strategy::SelectivePatch => Box::new(SelectivePatch),
    }
}

/// SHA-256 (lowercase hex) of the KB's canonical bytes (cards sorted, keys sorted).
pub fn kb_hash(kb: &ToolKb) -> String {
    hex::encode(Sha256::digest(kb.canonical_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------------------------

/// Keeps the original KB. Makes no provider call.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUpdate;

impl AdaptationStrategy for NoUpdate {
    fn strategy(&self) -> Strategy {
        Strategy::NoUpdate
    }

    fn adapt(&self, ctx: &mut AdaptationContext<'_, '_>) -> Result<Candidate, StrategyError> {
        let hash = kb_hash(ctx.source);
        Ok(Candidate {
            strategy: Strategy::NoUpdate,
            kb: ctx.source.clone(),
            prior_hash: hash.clone(),
            candidate_hash: hash,
            changed_lineage_ids: Vec::new(),
            changes: Vec::new(),
            patch: None,
        })
    }
}

/// Regenerates the complete KB from the target definitions.
#[derive(Debug, Clone, Copy, Default)]
pub struct FullRewrite;

const FULL_REWRITE_SYSTEM: &str = "You maintain a Tool KB (schema tool-kb/1) that documents MCP tools. \
Regenerate the complete knowledge base for the target server version from the tool definitions \
supplied by the user. Answer with exactly one JSON document that satisfies the supplied JSON \
Schema, with no code fences and no commentary. Include every tool exactly once. Copy lineage_id, \
server_id, tool_id, name, input_schema and output_schema exactly as given. Do not invent tools.";

impl AdaptationStrategy for FullRewrite {
    fn strategy(&self) -> Strategy {
        Strategy::FullRewrite
    }

    fn adapt(&self, ctx: &mut AdaptationContext<'_, '_>) -> Result<Candidate, StrategyError> {
        let schema: Value = serde_json::from_str(TOOL_KB_SCHEMA_JSON)
            .map_err(|e| StrategyError::Output(format!("internal: tool KB schema: {e}")))?;
        let user = format!(
            "Target server version: {}\nTarget tool definitions (canonical JSON):\n{}\n",
            ctx.target.server_version,
            canonical_json(&ctx.target.sorted())
        );
        let request = ctx.provider.decoding().request(
            "full_rewrite",
            FULL_REWRITE_SYSTEM.to_owned(),
            user,
            Some(schema),
        );
        let response = ctx.provider.call(Phase::Adaptation, &request)?;

        let value: Value = serde_json::from_str(response.text.trim())
            .map_err(|e| StrategyError::Output(format!("not JSON: {e}")))?;
        validate_tool_kb(&value).map_err(|errors| {
            StrategyError::Output(format!("not a valid tool KB: {}", errors.join("; ")))
        })?;
        let mut kb: ToolKb = serde_json::from_value(value)
            .map_err(|e| StrategyError::Output(format!("cannot decode tool KB: {e}")))?;

        stamp_metadata(&mut kb, ctx.target, Strategy::FullRewrite, None);
        validate_candidate_against_source(
            ctx.source,
            &kb,
            ctx.target,
            ctx.diff,
            Strategy::FullRewrite,
        )?;
        Ok(finish(Strategy::FullRewrite, kb, ctx, None))
    }
}

/// Applies a model-authored, diff-limited patch to the source KB.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectivePatch;

const SELECTIVE_PATCH_SYSTEM: &str = "You maintain a Tool KB (schema tool-kb/1) that documents MCP tools. \
Update the knowledge base for a new server version by answering with exactly one JSON patch \
document that satisfies the supplied JSON Schema, with no code fences and no commentary. Touch \
only the lineage ids and field paths listed under \"detected_changes\". Operations: set_field \
writes a value at a field path (for example description, tool_id, name, usage_rules, \
input_schema.properties.<name>, input_schema.required); remove_field deletes a nested key of \
input_schema or output_schema (a removed parameter); add_card adds the complete card of a new \
tool; remove_card removes a tool that no longer exists. Take values from the target definitions. \
Do not edit anything that is not listed.";

impl AdaptationStrategy for SelectivePatch {
    fn strategy(&self) -> Strategy {
        Strategy::SelectivePatch
    }

    fn adapt(&self, ctx: &mut AdaptationContext<'_, '_>) -> Result<Candidate, StrategyError> {
        let (patch, mut kb) = if ctx.diff.is_empty() {
            // Nothing changed: no model call is needed and none is made.
            (Patch::default(), ctx.source.clone())
        } else {
            let user = selective_patch_prompt(ctx);
            let request = ctx.provider.decoding().request(
                "selective_patch",
                SELECTIVE_PATCH_SYSTEM.to_owned(),
                user,
                Some(patch_json_schema()),
            );
            let response = ctx.provider.call(Phase::Adaptation, &request)?;
            let patch = parse_patch(&response.text)?;
            validate_patch(&patch, ctx.diff)?;
            let kb = apply_patch(ctx.source, &patch)?;
            (patch, kb)
        };

        stamp_metadata(
            &mut kb,
            ctx.target,
            Strategy::SelectivePatch,
            Some(&ctx.diff.affected_lineage_ids),
        );
        validate_candidate_against_source(
            ctx.source,
            &kb,
            ctx.target,
            ctx.diff,
            Strategy::SelectivePatch,
        )?;
        Ok(finish(Strategy::SelectivePatch, kb, ctx, Some(patch)))
    }
}

fn finish(
    strategy: Strategy,
    kb: ToolKb,
    ctx: &AdaptationContext<'_, '_>,
    patch: Option<Patch>,
) -> Candidate {
    Candidate {
        strategy,
        prior_hash: kb_hash(ctx.source),
        candidate_hash: kb_hash(&kb),
        changed_lineage_ids: ctx.diff.affected_lineage_ids.iter().cloned().collect(),
        changes: ctx.diff.changes.clone(),
        patch,
        kb,
    }
}

/// Deterministic user prompt: detected changes plus the source and target card of every
/// affected lineage.
fn selective_patch_prompt(ctx: &AdaptationContext<'_, '_>) -> String {
    let by_lineage = |kb: &'_ ToolKb| -> BTreeMap<String, ToolCard> {
        kb.cards
            .iter()
            .map(|c| (c.lineage_id.clone(), c.clone()))
            .collect()
    };
    let (src, tgt) = (by_lineage(ctx.source), by_lineage(ctx.target));
    let cards: Vec<Value> = ctx
        .diff
        .affected_lineage_ids
        .iter()
        .map(|id| {
            json!({
                "lineage_id": id,
                "source_card": src.get(id),
                "target_card": tgt.get(id),
            })
        })
        .collect();
    let changes = serde_json::to_value(&ctx.diff.changes).unwrap_or(Value::Null);
    format!(
        "Source server version: {}\nTarget server version: {}\ndetected_changes (canonical JSON):\n{}\naffected_cards (canonical JSON):\n{}\n",
        ctx.diff.source_version,
        ctx.diff.target_version,
        canonical_json(&changes),
        canonical_json(&Value::Array(cards)),
    )
}

/// Copy harness-owned metadata from the target definitions: KB header fields, and for every
/// card in `only` (or every card when `None`) that exists in the target, `server_version` and
/// `provenance`. Content fields are never modified.
fn stamp_metadata(
    kb: &mut ToolKb,
    target: &ToolKb,
    strategy: Strategy,
    only: Option<&BTreeSet<String>>,
) {
    kb.kb_id = format!("{}:{}", target.kb_id, strategy.as_str());
    kb.release_id = target.release_id.clone();
    kb.server_version = target.server_version.clone();
    let index: BTreeMap<(&str, &str), &ToolCard> = target
        .cards
        .iter()
        .map(|c| ((c.server_id.as_str(), c.lineage_id.as_str()), c))
        .collect();
    for card in &mut kb.cards {
        if only.is_some_and(|set| !set.contains(&card.lineage_id)) {
            continue;
        }
        if let Some(t) = index.get(&(card.server_id.as_str(), card.lineage_id.as_str())) {
            card.server_version = t.server_version.clone();
            card.provenance = t.provenance.clone();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

fn key(card: &ToolCard) -> (&str, &str) {
    (card.server_id.as_str(), card.lineage_id.as_str())
}

/// Validate a candidate against the target definitions, without needing the source KB.
///
/// * The candidate must be a schema-valid Tool KB (`tool-kb/1`) describing the target version.
/// * Every card must match a target card on `lineage_id`, `server_id`, `tool_id`, `name`,
///   `input_schema` and `output_schema`; no card may exist for a removed tool, no card may be
///   duplicated, and (`FullRewrite`, `SelectivePatch`) no target card may be missing.
/// * `SelectivePatch`: cards outside `diff.affected_lineage_ids` must also match the target's
///   `description` and `usage_rules` (they are unchanged between versions). Byte-identity with
///   the source is checked by [`validate_candidate_against_source`].
/// * `NoUpdate` candidates are only checked for schema validity here; they are not target
///   definitions by design.
pub fn validate_candidate(
    candidate: &ToolKb,
    target: &ToolKb,
    diff: &KbDiff,
    strategy: Strategy,
) -> Result<(), StrategyError> {
    let mut problems = Vec::new();

    match serde_json::to_value(candidate) {
        Ok(value) => {
            if let Err(errors) = validate_tool_kb(&value) {
                problems.extend(errors.into_iter().map(|e| format!("schema: {e}")));
            }
        }
        Err(e) => problems.push(format!("cannot serialize candidate: {e}")),
    }
    if !problems.is_empty() || strategy == Strategy::NoUpdate {
        return finish_validation(problems);
    }

    if candidate.server_version != target.server_version {
        problems.push(format!(
            "candidate describes server version {:?}, target is {:?}",
            candidate.server_version, target.server_version
        ));
    }

    let target_index: BTreeMap<(&str, &str), &ToolCard> =
        target.cards.iter().map(|c| (key(c), c)).collect();
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();

    for card in &candidate.cards {
        let id = key(card);
        if !seen.insert(id) {
            problems.push(format!("duplicate card {}", card.lineage_id));
            continue;
        }
        let Some(t) = target_index.get(&id) else {
            let why = if diff.is_removed(&card.lineage_id) {
                "the tool is removed in the target version"
            } else {
                "no such tool in the target version"
            };
            problems.push(format!("card {} must not exist: {why}", card.lineage_id));
            continue;
        };
        let structural: [(&str, bool); 4] = [
            ("tool_id", card.tool_id == t.tool_id),
            ("name", card.name == t.name),
            ("input_schema", card.input_schema == t.input_schema),
            ("output_schema", card.output_schema == t.output_schema),
        ];
        for (field, equal) in structural {
            if !equal {
                problems.push(format!(
                    "card {}: {field} differs from the target definition",
                    card.lineage_id
                ));
            }
        }
        if strategy == Strategy::SelectivePatch
            && !diff.affected_lineage_ids.contains(&card.lineage_id)
        {
            if card.description != t.description {
                problems.push(format!(
                    "card {}: unaffected card's description differs from the target",
                    card.lineage_id
                ));
            }
            if card.usage_rules != t.usage_rules {
                problems.push(format!(
                    "card {}: unaffected card's usage_rules differ from the target",
                    card.lineage_id
                ));
            }
        }
    }
    for t in &target.cards {
        if !seen.contains(&key(t)) {
            problems.push(format!(
                "card {} is missing from the candidate",
                t.lineage_id
            ));
        }
    }
    finish_validation(problems)
}

fn finish_validation(problems: Vec<String>) -> Result<(), StrategyError> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(StrategyError::Validation { problems })
    }
}

fn card_bytes(card: &ToolCard) -> String {
    canonical_json(card)
}

/// [`validate_candidate`] plus the checks that need the source KB:
///
/// * `NoUpdate`: the candidate is byte-identical to the source.
/// * `SelectivePatch`: every card outside `diff.affected_lineage_ids` is byte-identical
///   (canonical JSON) to its source card.
/// * `FullRewrite` and `SelectivePatch`: a detected description or usage-rule change must
///   actually be applied (the candidate field differs from the source field).
pub fn validate_candidate_against_source(
    source: &ToolKb,
    candidate: &ToolKb,
    target: &ToolKb,
    diff: &KbDiff,
    strategy: Strategy,
) -> Result<(), StrategyError> {
    validate_candidate(candidate, target, diff, strategy)?;
    let mut problems = Vec::new();

    if strategy == Strategy::NoUpdate {
        if candidate.canonical_bytes() != source.canonical_bytes() {
            problems.push("no-update candidate is not identical to the source KB".to_owned());
        }
        return finish_validation(problems);
    }

    let source_index: BTreeMap<(&str, &str), &ToolCard> =
        source.cards.iter().map(|c| (key(c), c)).collect();
    let candidate_index: BTreeMap<(&str, &str), &ToolCard> =
        candidate.cards.iter().map(|c| (key(c), c)).collect();

    if strategy == Strategy::SelectivePatch {
        for card in &candidate.cards {
            if diff.affected_lineage_ids.contains(&card.lineage_id) {
                continue;
            }
            match source_index.get(&key(card)) {
                Some(s) if card_bytes(s) == card_bytes(card) => {}
                Some(_) => problems.push(format!(
                    "unaffected card {} is not byte-identical to the source",
                    card.lineage_id
                )),
                None => problems.push(format!(
                    "unaffected card {} does not exist in the source",
                    card.lineage_id
                )),
            }
        }
    }

    for change in &diff.changes {
        if change.class != ChangeClass::Description || change.kind != ChangeKind::Modified {
            continue;
        }
        let Some(field) = change.field_path.as_deref() else {
            continue;
        };
        let pair = source
            .cards
            .iter()
            .find(|c| c.lineage_id == change.lineage_id)
            .and_then(|s| {
                candidate_index
                    .values()
                    .find(|c| c.lineage_id == change.lineage_id)
                    .map(|c| (s, *c))
            });
        let Some((s, c)) = pair else { continue };
        let unchanged = match field {
            "description" => s.description == c.description,
            "usage_rules" => s.usage_rules == c.usage_rules,
            _ => false,
        };
        if unchanged {
            problems.push(format!(
                "card {}: detected change to {field} was not applied",
                change.lineage_id
            ));
        }
    }
    finish_validation(problems)
}
