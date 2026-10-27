//! The three adaptation strategies and candidate validation.
//!
//! * [`NoUpdate`] returns the source KB unchanged and never calls a provider.
//! * [`FullRewrite`] asks the model to rewrite the description and usage rules of every tool
//!   from the target definitions.
//! * [`SelectivePatch`] asks the model for a structured [`Patch`] limited to the detected
//!   diff and applies it to the source KB.
//!
//! Model output is never broadened. Model calls are made in batches of at most
//! `ADAPTATION_BATCH_CARDS` cards; a batch whose answer does not parse or validate is retried
//! with the rejection reason, up to `ADAPTATION_ATTEMPTS` times, and then the strategy returns
//! a [`StrategyError`]. The merged candidate is validated again as a whole. Prompts are built only from the
//! diff and the target definitions; task text is never used to infer changes.
//!
//! Harness-owned fields (KB header fields, and the structure, `server_version` and `provenance`
//! of cards the candidate updated) are copied from the target definitions after the model
//! answered. The model is not trusted with those values; it authors descriptions and usage
//! rules, which are never touched.

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
use crate::schema::validate_tool_kb;

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
    /// The model-authored patch (selective patch only). The candidate is this patch applied to
    /// the source, followed by the harness copying structure, version and provenance from the
    /// target for affected cards, so replaying the patch alone does not reproduce `kb`.
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
Rewrite the documentation of every tool supplied by the user for the target server version: a \
description and usage rules that are accurate for the given definition. Answer with exactly one \
JSON document {\"cards\": [{\"lineage_id\", \"description\", \"usage_rules\"}]} that satisfies the \
supplied JSON Schema, with no code fences and no commentary. Include every tool exactly once, \
using its lineage_id as given. Do not invent tools. Identity, schemas and versions are copied from \
the definitions by the harness.";

/// JSON Schema of a full-rewrite answer: the model-authored fields of every card.
fn rewrite_json_schema() -> Value {
    json!({
        "type": "object",
        "required": ["cards"],
        "properties": {
            "cards": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["lineage_id", "description", "usage_rules"],
                    "properties": {
                        "lineage_id": { "type": "string" },
                        "description": { "type": "string" },
                        "usage_rules": { "type": "array", "items": { "type": "string" } }
                    }
                }
            }
        }
    })
}

/// Apply a full-rewrite answer to the target definitions in `part`: each target card takes the
/// answer's `description` and `usage_rules`; everything else comes from the target. Fields the
/// answer carries beyond those three are ignored.
fn apply_rewrite(text: &str, part: &ToolKb) -> Result<ToolKb, StrategyError> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| StrategyError::Output(format!("not JSON: {e}")))?;
    let Some(cards) = value.get("cards").and_then(Value::as_array) else {
        return Err(StrategyError::Output(
            "expected an object with a cards array".to_owned(),
        ));
    };
    let mut authored: BTreeMap<&str, (String, Vec<String>)> = BTreeMap::new();
    let mut problems = Vec::new();
    for (index, card) in cards.iter().enumerate() {
        let lineage = card.get("lineage_id").and_then(Value::as_str);
        let description = card.get("description").and_then(Value::as_str);
        let rules: Option<Vec<String>> = card
            .get("usage_rules")
            .and_then(Value::as_array)
            .and_then(|rules| {
                rules
                    .iter()
                    .map(|r| r.as_str().map(str::to_owned))
                    .collect()
            });
        let (Some(lineage), Some(description), Some(rules)) = (lineage, description, rules) else {
            return Err(StrategyError::Output(format!(
                "/cards/{index}: lineage_id, description and usage_rules (strings) are required"
            )));
        };
        if !part.cards.iter().any(|c| c.lineage_id == lineage) {
            problems.push(format!(
                "card {lineage} must not exist: no such tool in the target version"
            ));
        } else if authored
            .insert(lineage, (description.to_owned(), rules))
            .is_some()
        {
            problems.push(format!("duplicate card {lineage}"));
        }
    }
    let mut kb = part.clone();
    for card in &mut kb.cards {
        match authored.remove(card.lineage_id.as_str()) {
            Some((description, rules)) => {
                card.description = description;
                card.usage_rules = rules;
            }
            None => problems.push(format!(
                "card {} is missing from the candidate",
                card.lineage_id
            )),
        }
    }
    finish_validation(problems)?;
    Ok(kb)
}

impl AdaptationStrategy for FullRewrite {
    fn strategy(&self) -> Strategy {
        Strategy::FullRewrite
    }

    /// One model call per batch of at most [`ADAPTATION_BATCH_CARDS`] cards of one server: a whole
    /// multi-server KB can exceed a single response's output limit and the request timeout. The merged cards are validated against the source and target as one candidate.
    fn adapt(&self, ctx: &mut AdaptationContext<'_, '_>) -> Result<Candidate, StrategyError> {
        let target = ctx.target.sorted();
        let mut servers: Vec<&str> = target.cards.iter().map(|c| c.server_id.as_str()).collect();
        servers.dedup();
        let mut kb: Option<ToolKb> = None;
        let mut parts = Vec::new();
        for server in servers {
            let cards: Vec<ToolCard> = target
                .cards
                .iter()
                .filter(|c| c.server_id == server)
                .cloned()
                .collect();
            for chunk in cards.chunks(ADAPTATION_BATCH_CARDS) {
                let mut part = target.clone();
                part.cards = chunk.to_vec();
                parts.push((server, part));
            }
        }
        for (server, part) in parts {
            let ids: BTreeSet<String> = part.cards.iter().map(|c| c.lineage_id.clone()).collect();
            let batch_diff = restrict_diff(ctx.diff, &ids);
            let batch_source = restrict_kb(ctx.source, &ids);
            let base_user = format!(
                "Target server version: {}\nTarget tool definitions (canonical JSON):\n{}\n",
                ctx.target.server_version,
                canonical_json(&part)
            );
            let rewritten = with_retries(|feedback| {
                let request = ctx.provider.decoding().request(
                    "full_rewrite",
                    FULL_REWRITE_SYSTEM.to_owned(),
                    with_feedback(&base_user, feedback),
                    Some(rewrite_json_schema()),
                );
                let response = ctx.provider.call(Phase::Adaptation, &request)?;
                let mut rewritten = apply_rewrite(&response.text, &part).map_err(|e| match e {
                    StrategyError::Output(m) => StrategyError::Output(format!("{server}: {m}")),
                    other => other,
                })?;
                stamp_metadata(&mut rewritten, &part, Strategy::FullRewrite, None);
                validate_candidate_against_source(
                    &batch_source,
                    &rewritten,
                    &part,
                    &batch_diff,
                    Strategy::FullRewrite,
                )?;
                Ok(rewritten)
            })?;
            match &mut kb {
                Some(kb) => kb.cards.extend(rewritten.cards),
                None => kb = Some(rewritten),
            }
        }
        let Some(mut kb) = kb else {
            return Err(StrategyError::Output("target KB has no tools".to_owned()));
        };

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
            // Bounded batches of affected lineages keep each response within the request
            // timeout; every batch patch is checked against its own slice of the diff.
            let mut patch = Patch::default();
            for batch in diff_batches(ctx.diff, ADAPTATION_BATCH_CARDS) {
                let base_user = selective_patch_prompt(ctx.source, ctx.target, &batch);
                let ids = &batch.affected_lineage_ids;
                let (batch_source, batch_target) =
                    (restrict_kb(ctx.source, ids), restrict_kb(ctx.target, ids));
                let part = with_retries(|feedback| {
                    let request = ctx.provider.decoding().request(
                        "selective_patch",
                        SELECTIVE_PATCH_SYSTEM.to_owned(),
                        with_feedback(&base_user, feedback),
                        Some(patch_json_schema()),
                    );
                    let response = ctx.provider.call(Phase::Adaptation, &request)?;
                    let part = parse_patch(&response.text)?;
                    validate_patch(&part, &batch)?;
                    let mut patched = apply_patch(&batch_source, &part)?;
                    stamp_metadata(
                        &mut patched,
                        &batch_target,
                        Strategy::SelectivePatch,
                        Some(ids),
                    );
                    validate_candidate_against_source(
                        &batch_source,
                        &patched,
                        &batch_target,
                        &batch,
                        Strategy::SelectivePatch,
                    )?;
                    Ok(part)
                })?;
                patch.ops.extend(part.ops);
            }
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

/// Most cards sent to the model in one adaptation request.
const ADAPTATION_BATCH_CARDS: usize = 12;

/// Attempts per adaptation batch. A rejected answer is retried with the rejection reason;
/// provider and budget failures are never retried here.
const ADAPTATION_ATTEMPTS: usize = 3;

fn with_retries<T>(
    mut attempt: impl FnMut(Option<&str>) -> Result<T, StrategyError>,
) -> Result<T, StrategyError> {
    let mut feedback: Option<String> = None;
    for remaining in (0..ADAPTATION_ATTEMPTS).rev() {
        match attempt(feedback.as_deref()) {
            Err(StrategyError::Provider(e)) => return Err(StrategyError::Provider(e)),
            Err(e) if remaining > 0 => feedback = Some(e.to_string()),
            result => return result,
        }
    }
    unreachable!("ADAPTATION_ATTEMPTS is at least one")
}

fn with_feedback(user: &str, feedback: Option<&str>) -> String {
    match feedback {
        None => user.to_owned(),
        Some(reason) => format!(
            "{user}\nYour previous answer was rejected by the harness: {reason}\nAnswer again, fixing every listed problem.\n"
        ),
    }
}

/// The cards of `kb` whose lineage is in `ids`, with the same header.
fn restrict_kb(kb: &ToolKb, ids: &BTreeSet<String>) -> ToolKb {
    let mut part = kb.clone();
    part.cards.retain(|c| ids.contains(&c.lineage_id));
    part
}

/// The changes of `diff` for lineages in `ids`.
fn restrict_diff(diff: &KbDiff, ids: &BTreeSet<String>) -> KbDiff {
    KbDiff {
        source_version: diff.source_version.clone(),
        target_version: diff.target_version.clone(),
        changes: diff
            .changes
            .iter()
            .filter(|c| ids.contains(&c.lineage_id))
            .cloned()
            .collect(),
        affected_lineage_ids: diff
            .affected_lineage_ids
            .intersection(ids)
            .cloned()
            .collect(),
    }
}

/// Split a diff into sub-diffs of at most `size` affected lineages, in lineage order.
fn diff_batches(diff: &KbDiff, size: usize) -> Vec<KbDiff> {
    let ids: Vec<&String> = diff.affected_lineage_ids.iter().collect();
    ids.chunks(size.max(1))
        .map(|chunk| restrict_diff(diff, &chunk.iter().map(|id| (*id).clone()).collect()))
        .collect()
}

/// Deterministic user prompt: detected changes plus the source and target card of every
/// affected lineage in `diff`.
fn selective_patch_prompt(source: &ToolKb, target: &ToolKb, diff: &KbDiff) -> String {
    let by_lineage = |kb: &'_ ToolKb| -> BTreeMap<String, ToolCard> {
        kb.cards
            .iter()
            .map(|c| (c.lineage_id.clone(), c.clone()))
            .collect()
    };
    let (src, tgt) = (by_lineage(source), by_lineage(target));
    let cards: Vec<Value> = diff
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
    let changes = serde_json::to_value(&diff.changes).unwrap_or(Value::Null);
    format!(
        "Source server version: {}\nTarget server version: {}\ndetected_changes (canonical JSON):\n{}\naffected_cards (canonical JSON):\n{}\n",
        diff.source_version,
        diff.target_version,
        canonical_json(&changes),
        canonical_json(&Value::Array(cards)),
    )
}

/// Copy harness-owned fields from the target definitions: KB header fields, and for every card
/// in `only` (or every card when `None`) that exists in the target, its structure (`tool_id`,
/// `name`, `input_schema`, `output_schema`), `server_version` and `provenance`. Validation
/// requires the structure to equal the target, so a model transcription could only add errors.
/// Model-authored content (`description`, `usage_rules`) is never modified.
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
            card.tool_id = t.tool_id.clone();
            card.name = t.name.clone();
            card.input_schema = t.input_schema.clone();
            card.output_schema = t.output_schema.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn change(lineage: &str, field: &str) -> ChangeRecord {
        ChangeRecord {
            lineage_id: lineage.to_owned(),
            tool_id: lineage.to_owned(),
            class: ChangeClass::Description,
            kind: ChangeKind::Modified,
            field_path: Some(field.to_owned()),
            old_hash: None,
            new_hash: None,
        }
    }

    #[test]
    fn diff_batches_partition_lineages_and_keep_their_changes() {
        let changes = vec![
            change("a", "description"),
            change("a", "input_schema.properties.x"),
            change("b", "description"),
            change("c", "description"),
        ];
        let diff = KbDiff {
            source_version: "early".into(),
            target_version: "late".into(),
            affected_lineage_ids: changes.iter().map(|c| c.lineage_id.clone()).collect(),
            changes,
        };
        let batches = diff_batches(&diff, 2);
        assert_eq!(batches.len(), 2);
        let ids: Vec<Vec<&str>> = batches
            .iter()
            .map(|b| b.affected_lineage_ids.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(ids, vec![vec!["a", "b"], vec!["c"]]);
        assert_eq!(batches[0].changes.len(), 3);
        assert_eq!(batches[1].changes.len(), 1);
        assert!(batches.iter().all(|b| b.target_version == "late"));
        assert!(
            diff_batches(
                &KbDiff {
                    changes: vec![],
                    affected_lineage_ids: BTreeSet::new(),
                    ..diff
                },
                2
            )
            .is_empty()
        );
    }
}
