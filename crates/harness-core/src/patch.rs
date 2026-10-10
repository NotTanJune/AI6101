//! Structured patch type, validation against a diff, and application.
//!
//! A [`Patch`] is what the selective-patch strategy accepts from a model. It is deliberately
//! narrow: every operation names a lineage id and, for field operations, a field path. Before
//! a patch is applied, [`validate_patch`] rejects any operation that touches a lineage or a
//! field outside the detected changes. A patch is never widened or repaired: it is accepted
//! as written or rejected.
//!
//! Field paths use the same syntax as `ChangeRecord::field_path` (see
//! [`crate::diff::escape_path_segment`]). Operation `set_field` writes the value at a path;
//! `remove_field` deletes a nested key (needed for removed parameters); `add_card` and
//! `remove_card` add or remove whole cards.

use std::collections::BTreeSet;

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::diff::{KbDiff, unescape_path_segment};
use crate::model::{ToolCard, ToolKb};
use crate::schema::TOOL_KB_SCHEMA_JSON;

/// Top-level card fields a patch may address.
const PATCHABLE_FIELDS: [&str; 6] = [
    "tool_id",
    "name",
    "description",
    "usage_rules",
    "input_schema",
    "output_schema",
];

/// One patch operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchOp {
    /// Write `value` at `field_path` of the card with `lineage_id`.
    SetField {
        /// Lineage id of the card.
        lineage_id: String,
        /// Path such as `description` or `input_schema.properties.limit`.
        field_path: String,
        /// New value.
        value: Value,
    },
    /// Add a whole card for a tool that is new in the target version.
    AddCard {
        /// The complete card.
        card: Box<ToolCard>,
    },
    /// Remove the card of a tool that no longer exists in the target version.
    RemoveCard {
        /// Lineage id of the card.
        lineage_id: String,
    },
    /// Delete a nested key of `input_schema` or `output_schema` (for example a removed
    /// parameter).
    RemoveField {
        /// Lineage id of the card.
        lineage_id: String,
        /// Path of at least two segments below `input_schema` or `output_schema`.
        field_path: String,
    },
}

/// A structured patch: operations applied in order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    /// Operations, applied in order.
    pub ops: Vec<PatchOp>,
}

/// Patch parsing, validation or application failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// The text is not a schema-valid patch document.
    #[error("patch output is not a valid patch document: {0}")]
    Malformed(String),
    /// An operation touches a lineage or field that the diff does not list.
    #[error("operation {index} is outside the detected changes: {reason}")]
    OutsideDiff {
        /// Zero-based operation index.
        index: usize,
        /// What was rejected and why.
        reason: String,
    },
    /// An operation addresses a field a patch may never write.
    #[error("operation {index} addresses forbidden field {field_path:?}")]
    ForbiddenField {
        /// Zero-based operation index.
        index: usize,
        /// The offending path.
        field_path: String,
    },
    /// Two operations address the same card or field.
    #[error("operation {index} duplicates an earlier operation on {target}")]
    Duplicate {
        /// Zero-based operation index.
        index: usize,
        /// Lineage id (and path) addressed twice.
        target: String,
    },
    /// The lineage id does not exist in the KB being patched.
    #[error("operation {index}: no card with lineage id {lineage_id:?}")]
    UnknownCard {
        /// Zero-based operation index.
        index: usize,
        /// Missing lineage id.
        lineage_id: String,
    },
    /// The lineage id matches more than one card, so the operation is ambiguous.
    #[error("operation {index}: lineage id {lineage_id:?} matches several cards")]
    AmbiguousCard {
        /// Zero-based operation index.
        index: usize,
        /// Ambiguous lineage id.
        lineage_id: String,
    },
    /// `add_card` for a lineage id that already exists.
    #[error("operation {index}: card {lineage_id:?} already exists")]
    CardExists {
        /// Zero-based operation index.
        index: usize,
        /// Existing lineage id.
        lineage_id: String,
    },
    /// The field path does not resolve to an existing container.
    #[error("operation {index}: invalid field path {field_path:?}: {reason}")]
    InvalidPath {
        /// Zero-based operation index.
        index: usize,
        /// The offending path.
        field_path: String,
        /// Why it cannot be applied.
        reason: String,
    },
    /// The value has the wrong type for the addressed field.
    #[error("operation {index}: invalid value for {field_path:?}: {reason}")]
    InvalidValue {
        /// Zero-based operation index.
        index: usize,
        /// The offending path.
        field_path: String,
        /// Why the value was rejected.
        reason: String,
    },
}

// ---------------------------------------------------------------------------------------------
// JSON Schema and parsing
// ---------------------------------------------------------------------------------------------

/// JSON Schema (draft 2020-12) of a patch document, for structured model output.
///
/// The card definition is taken from the Tool KB schema so both stay in step.
pub fn patch_json_schema() -> Value {
    let kb_schema: Value = serde_json::from_str(TOOL_KB_SCHEMA_JSON).unwrap_or(Value::Null);
    let kb_defs = kb_schema.get("$defs").cloned().unwrap_or_else(|| json!({}));
    let mut defs = kb_defs.as_object().cloned().unwrap_or_default();
    let nonempty = json!({ "$ref": "#/$defs/nonempty_string" });
    defs.insert(
        "op".to_owned(),
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["op", "lineage_id", "field_path", "value"],
                    "properties": {
                        "op": { "const": "set_field" },
                        "lineage_id": nonempty,
                        "field_path": nonempty,
                        "value": {}
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["op", "card"],
                    "properties": {
                        "op": { "const": "add_card" },
                        "card": { "$ref": "#/$defs/card" }
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["op", "lineage_id"],
                    "properties": {
                        "op": { "const": "remove_card" },
                        "lineage_id": nonempty
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["op", "lineage_id", "field_path"],
                    "properties": {
                        "op": { "const": "remove_field" },
                        "lineage_id": nonempty,
                        "field_path": nonempty
                    }
                }
            ]
        }),
    );
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Tool KB patch",
        "type": "object",
        "additionalProperties": false,
        "required": ["ops"],
        "properties": {
            "ops": { "type": "array", "items": { "$ref": "#/$defs/op" } }
        },
        "$defs": Value::Object(defs)
    })
}

fn patch_validator() -> Result<&'static Validator, PatchError> {
    static V: std::sync::OnceLock<Result<Validator, String>> = std::sync::OnceLock::new();
    V.get_or_init(|| jsonschema::validator_for(&patch_json_schema()).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| PatchError::Malformed(format!("internal error: patch schema: {e}")))
}

/// Parse model output into a [`Patch`]. The text must be exactly one JSON document that
/// satisfies [`patch_json_schema`]; code fences or surrounding prose are rejected.
pub fn parse_patch(text: &str) -> Result<Patch, PatchError> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| PatchError::Malformed(format!("not JSON: {e}")))?;
    let validator = patch_validator()?;
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| {
            let path = e.instance_path().to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .collect();
    if !errors.is_empty() {
        return Err(PatchError::Malformed(errors.join("; ")));
    }
    serde_json::from_value(value).map_err(|e| PatchError::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

fn segments(path: &str) -> Vec<&str> {
    path.split('.').collect()
}

/// Whether a change at `change_path` covers an operation at `op_path`: the operation must
/// address the changed path itself or something below it, never an ancestor.
fn path_covers(change_path: &str, op_path: &str) -> bool {
    let change = segments(change_path);
    let op = segments(op_path);
    op.len() >= change.len() && op[..change.len()] == change[..]
}

fn check_field_path(index: usize, field_path: &str) -> Result<(), PatchError> {
    let segs = segments(field_path);
    if segs.iter().any(|s| s.is_empty()) {
        return Err(PatchError::InvalidPath {
            index,
            field_path: field_path.to_owned(),
            reason: "empty path segment".to_owned(),
        });
    }
    if !PATCHABLE_FIELDS.contains(&segs[0]) {
        return Err(PatchError::ForbiddenField {
            index,
            field_path: field_path.to_owned(),
        });
    }
    Ok(())
}

/// Reject any operation that touches a lineage or field outside the detected changes.
///
/// * `set_field` / `remove_field` need a change record on the same lineage whose field path is
///   the operation's path or an ancestor of it. Cards that the diff adds or removes as a whole
///   cannot be edited field by field.
/// * `add_card` needs an `added` whole-card change; `remove_card` a `removed` one.
/// * Two operations on the same card (for `add`/`remove`) or the same field are rejected.
///
/// An empty patch is valid here; whether it is complete is decided when the candidate is
/// validated against the target.
pub fn validate_patch(patch: &Patch, diff: &KbDiff) -> Result<(), PatchError> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, op) in patch.ops.iter().enumerate() {
        match op {
            PatchOp::SetField {
                lineage_id,
                field_path,
                ..
            }
            | PatchOp::RemoveField {
                lineage_id,
                field_path,
            } => {
                check_field_path(index, field_path)?;
                if let PatchOp::RemoveField { .. } = op {
                    let segs = segments(field_path);
                    if segs.len() < 2 || !matches!(segs[0], "input_schema" | "output_schema") {
                        return Err(PatchError::ForbiddenField {
                            index,
                            field_path: field_path.clone(),
                        });
                    }
                }
                if !diff.affected_lineage_ids.contains(lineage_id) {
                    return Err(PatchError::OutsideDiff {
                        index,
                        reason: format!("lineage {lineage_id:?} has no detected change"),
                    });
                }
                if diff.is_added(lineage_id) || diff.is_removed(lineage_id) {
                    return Err(PatchError::OutsideDiff {
                        index,
                        reason: format!(
                            "lineage {lineage_id:?} is added or removed as a whole; use add_card or remove_card"
                        ),
                    });
                }
                let covered = diff.changes_for(lineage_id).any(|c| {
                    c.field_path
                        .as_deref()
                        .is_some_and(|p| path_covers(p, field_path))
                });
                if !covered {
                    return Err(PatchError::OutsideDiff {
                        index,
                        reason: format!(
                            "field {field_path:?} of {lineage_id:?} is not a detected change"
                        ),
                    });
                }
                let key = format!("field:{lineage_id}:{field_path}");
                if !seen.insert(key) {
                    return Err(PatchError::Duplicate {
                        index,
                        target: format!("{lineage_id}.{field_path}"),
                    });
                }
            }
            PatchOp::AddCard { card } => {
                if !diff.is_added(&card.lineage_id) {
                    return Err(PatchError::OutsideDiff {
                        index,
                        reason: format!("lineage {:?} is not added by the diff", card.lineage_id),
                    });
                }
                if !seen.insert(format!("card:{}", card.lineage_id)) {
                    return Err(PatchError::Duplicate {
                        index,
                        target: card.lineage_id.clone(),
                    });
                }
            }
            PatchOp::RemoveCard { lineage_id } => {
                if !diff.is_removed(lineage_id) {
                    return Err(PatchError::OutsideDiff {
                        index,
                        reason: format!("lineage {lineage_id:?} is not removed by the diff"),
                    });
                }
                if !seen.insert(format!("card:{lineage_id}")) {
                    return Err(PatchError::Duplicate {
                        index,
                        target: lineage_id.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------------------------

fn find_card_mut<'a>(
    cards: &'a mut [ToolCard],
    index: usize,
    lineage_id: &str,
) -> Result<&'a mut ToolCard, PatchError> {
    let mut hits = cards.iter_mut().filter(|c| c.lineage_id == lineage_id);
    let first = hits.next().ok_or_else(|| PatchError::UnknownCard {
        index,
        lineage_id: lineage_id.to_owned(),
    })?;
    if hits.next().is_some() {
        return Err(PatchError::AmbiguousCard {
            index,
            lineage_id: lineage_id.to_owned(),
        });
    }
    Ok(first)
}

fn invalid_path(index: usize, field_path: &str, reason: &str) -> PatchError {
    PatchError::InvalidPath {
        index,
        field_path: field_path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn invalid_value(index: usize, field_path: &str, reason: &str) -> PatchError {
    PatchError::InvalidValue {
        index,
        field_path: field_path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Walk `segs[..len-1]` inside `root` and return the object that holds the last segment.
fn parent_object<'a>(
    root: &'a mut Value,
    segs: &[String],
    index: usize,
    field_path: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, PatchError> {
    let mut current = root;
    for seg in &segs[..segs.len() - 1] {
        current = current
            .as_object_mut()
            .and_then(|o| o.get_mut(seg))
            .ok_or_else(|| invalid_path(index, field_path, &format!("no object at {seg:?}")))?;
    }
    current
        .as_object_mut()
        .ok_or_else(|| invalid_path(index, field_path, "parent is not an object"))
}

fn set_field(
    card: &mut ToolCard,
    index: usize,
    field_path: &str,
    value: &Value,
) -> Result<(), PatchError> {
    let raw = segments(field_path);
    let segs: Vec<String> = raw.iter().map(|s| unescape_path_segment(s)).collect();
    let need_string = |v: &Value| -> Result<String, PatchError> {
        v.as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid_value(index, field_path, "expected a string"))
    };
    match (raw[0], raw.len()) {
        ("tool_id", 1) => {
            let s = need_string(value)?;
            if s.is_empty() {
                return Err(invalid_value(index, field_path, "must not be empty"));
            }
            card.tool_id = s;
        }
        ("name", 1) => {
            let s = need_string(value)?;
            if s.is_empty() {
                return Err(invalid_value(index, field_path, "must not be empty"));
            }
            card.name = s;
        }
        ("description", 1) => card.description = need_string(value)?,
        ("usage_rules", 1) => {
            let rules = value
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| invalid_value(index, field_path, "expected an array of strings"))?;
            card.usage_rules = rules;
        }
        ("input_schema", 1) => {
            if !value.is_object() {
                return Err(invalid_value(index, field_path, "expected an object"));
            }
            card.input_schema = value.clone();
        }
        ("output_schema", 1) => {
            if !(value.is_object() || value.is_null()) {
                return Err(invalid_value(
                    index,
                    field_path,
                    "expected an object or null",
                ));
            }
            card.output_schema = (!value.is_null()).then(|| value.clone());
        }
        ("input_schema", _) => {
            let last = segs[segs.len() - 1].clone();
            let parent = parent_object(&mut card.input_schema, &segs[1..], index, field_path)?;
            parent.insert(last, value.clone());
        }
        ("output_schema", _) => {
            let root = card
                .output_schema
                .as_mut()
                .ok_or_else(|| invalid_path(index, field_path, "output_schema is null"))?;
            let last = segs[segs.len() - 1].clone();
            let parent = parent_object(root, &segs[1..], index, field_path)?;
            parent.insert(last, value.clone());
        }
        _ => {
            return Err(invalid_path(
                index,
                field_path,
                "this field has no nested paths",
            ));
        }
    }
    Ok(())
}

fn remove_field(card: &mut ToolCard, index: usize, field_path: &str) -> Result<(), PatchError> {
    let raw = segments(field_path);
    let segs: Vec<String> = raw.iter().map(|s| unescape_path_segment(s)).collect();
    if segs.len() < 2 {
        return Err(invalid_path(
            index,
            field_path,
            "needs at least two segments",
        ));
    }
    let root = match raw[0] {
        "input_schema" => &mut card.input_schema,
        "output_schema" => card
            .output_schema
            .as_mut()
            .ok_or_else(|| invalid_path(index, field_path, "output_schema is null"))?,
        _ => {
            return Err(invalid_path(
                index,
                field_path,
                "only schemas have nested keys",
            ));
        }
    };
    let last = &segs[segs.len() - 1];
    let parent = parent_object(root, &segs[1..], index, field_path)?;
    if parent.remove(last).is_none() {
        return Err(invalid_path(index, field_path, "key does not exist"));
    }
    Ok(())
}

/// Apply `patch` to a copy of `kb`. Operations are applied in order; the first failure aborts
/// and nothing is returned (the input is never modified). Card order is preserved, added cards
/// are appended.
///
/// This does not check the patch against a diff; call [`validate_patch`] first.
pub fn apply_patch(kb: &ToolKb, patch: &Patch) -> Result<ToolKb, PatchError> {
    let mut out = kb.clone();
    for (index, op) in patch.ops.iter().enumerate() {
        match op {
            PatchOp::SetField {
                lineage_id,
                field_path,
                value,
            } => {
                check_field_path(index, field_path)?;
                let card = find_card_mut(&mut out.cards, index, lineage_id)?;
                set_field(card, index, field_path, value)?;
            }
            PatchOp::RemoveField {
                lineage_id,
                field_path,
            } => {
                check_field_path(index, field_path)?;
                let card = find_card_mut(&mut out.cards, index, lineage_id)?;
                remove_field(card, index, field_path)?;
            }
            PatchOp::AddCard { card } => {
                if out.cards.iter().any(|c| c.lineage_id == card.lineage_id) {
                    return Err(PatchError::CardExists {
                        index,
                        lineage_id: card.lineage_id.clone(),
                    });
                }
                out.cards.push((**card).clone());
            }
            PatchOp::RemoveCard { lineage_id } => {
                let before = out.cards.len();
                out.cards.retain(|c| &c.lineage_id != lineage_id);
                match before - out.cards.len() {
                    0 => {
                        return Err(PatchError::UnknownCard {
                            index,
                            lineage_id: lineage_id.clone(),
                        });
                    }
                    1 => {}
                    _ => {
                        return Err(PatchError::AmbiguousCard {
                            index,
                            lineage_id: lineage_id.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_descendants_not_ancestors() {
        assert!(path_covers(
            "input_schema.required",
            "input_schema.required"
        ));
        assert!(path_covers(
            "input_schema.properties.x",
            "input_schema.properties.x.type"
        ));
        assert!(!path_covers("input_schema.properties.x", "input_schema"));
        assert!(!path_covers(
            "input_schema.properties.x",
            "input_schema.properties"
        ));
        assert!(!path_covers(
            "input_schema.properties.x",
            "input_schema.properties.xy"
        ));
    }

    #[test]
    fn patch_schema_compiles() {
        assert!(patch_validator().is_ok());
    }

    #[test]
    fn empty_ops_parse() {
        assert_eq!(parse_patch(r#"{"ops":[]}"#), Ok(Patch::default()));
    }
}
