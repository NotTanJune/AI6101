//! Version diff and change classification between two Tool KBs.
//!
//! Cards are matched by `(server_id, lineage_id)`, so a rename that keeps its lineage id is
//! reported as a rename rather than a remove plus an add. Only content that an agent can act
//! on is compared: `tool_id`, `name`, `input_schema`, `output_schema`, `description` and
//! `usage_rules`. The card's `server_version` and `provenance` differ between any two versions
//! by construction and are never reported as changes.
//!
//! Field paths use `.` between segments. A literal `.` or `~` inside a JSON property name is
//! escaped as `~1` and `~0` (see [`escape_path_segment`]).
//!
//! The diff never looks at task text: a change exists only when the two KBs differ.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::events::{ChangeKind, ChangeRecord};
use crate::model::{ChangeClass, ToolCard, ToolKb, canonical_json, card_hash};

/// Result of comparing a source KB with a target KB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KbDiff {
    /// Version label of the source KB.
    pub source_version: String,
    /// Version label of the target KB.
    pub target_version: String,
    /// Detected changes in deterministic order (by server, lineage, then field).
    pub changes: Vec<ChangeRecord>,
    /// Lineage ids touched by at least one change (added, removed, renamed or modified).
    pub affected_lineage_ids: BTreeSet<String>,
}

impl KbDiff {
    /// Whether the two KBs are equivalent for the compared fields.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Changes recorded for one lineage id.
    pub fn changes_for<'a>(
        &'a self,
        lineage_id: &'a str,
    ) -> impl Iterator<Item = &'a ChangeRecord> + 'a {
        self.changes
            .iter()
            .filter(move |c| c.lineage_id == lineage_id)
    }

    /// Whether `lineage_id` is added by this diff (a whole-card change of kind `added`).
    pub fn is_added(&self, lineage_id: &str) -> bool {
        self.changes_for(lineage_id)
            .any(|c| c.kind == ChangeKind::Added && c.field_path.is_none())
    }

    /// Whether `lineage_id` is removed by this diff (a whole-card change of kind `removed`).
    pub fn is_removed(&self, lineage_id: &str) -> bool {
        self.changes_for(lineage_id)
            .any(|c| c.kind == ChangeKind::Removed && c.field_path.is_none())
    }
}

/// Escape one path segment: `~` becomes `~0` and `.` becomes `~1`.
pub fn escape_path_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('.', "~1")
}

/// Inverse of [`escape_path_segment`].
pub fn unescape_path_segment(segment: &str) -> String {
    segment.replace("~1", ".").replace("~0", "~")
}

/// SHA-256 (lowercase hex) of the canonical JSON of a field value.
pub fn field_hash(value: &Value) -> String {
    hex::encode(Sha256::digest(canonical_json(value).as_bytes()))
}

/// Compare `source` with `target`.
///
/// If a KB lists the same `(server_id, lineage_id)` twice, the last card wins; validate KBs
/// (for example with `validate_candidate`) before relying on their diff.
pub fn diff_kbs(source: &ToolKb, target: &ToolKb) -> KbDiff {
    let src = index(source);
    let tgt = index(target);
    let keys: BTreeSet<&(String, String)> = src.keys().chain(tgt.keys()).collect();

    let mut changes = Vec::new();
    for key in keys {
        match (src.get(key), tgt.get(key)) {
            (None, Some(t)) => changes.push(ChangeRecord {
                lineage_id: t.lineage_id.clone(),
                tool_id: t.tool_id.clone(),
                class: ChangeClass::Tool,
                kind: ChangeKind::Added,
                field_path: None,
                old_hash: None,
                new_hash: Some(card_hash(t)),
            }),
            (Some(s), None) => changes.push(ChangeRecord {
                lineage_id: s.lineage_id.clone(),
                tool_id: s.tool_id.clone(),
                class: ChangeClass::Tool,
                kind: ChangeKind::Removed,
                field_path: None,
                old_hash: Some(card_hash(s)),
                new_hash: None,
            }),
            (Some(s), Some(t)) => diff_card(s, t, &mut changes),
            (None, None) => {}
        }
    }

    let affected_lineage_ids = changes.iter().map(|c| c.lineage_id.clone()).collect();
    KbDiff {
        source_version: source.server_version.clone(),
        target_version: target.server_version.clone(),
        changes,
        affected_lineage_ids,
    }
}

fn index(kb: &ToolKb) -> BTreeMap<(String, String), &ToolCard> {
    kb.cards
        .iter()
        .map(|c| ((c.server_id.clone(), c.lineage_id.clone()), c))
        .collect()
}

/// Builder for records of one card pair.
struct CardChanges<'a> {
    target: &'a ToolCard,
    out: &'a mut Vec<ChangeRecord>,
}

impl CardChanges<'_> {
    fn push(
        &mut self,
        class: ChangeClass,
        kind: ChangeKind,
        path: String,
        old: Option<&Value>,
        new: Option<&Value>,
    ) {
        self.out.push(ChangeRecord {
            lineage_id: self.target.lineage_id.clone(),
            tool_id: self.target.tool_id.clone(),
            class,
            kind,
            field_path: Some(path),
            old_hash: old.map(field_hash),
            new_hash: new.map(field_hash),
        });
    }

    /// Record a change of an optional value: added, removed or modified.
    fn push_optional(
        &mut self,
        class: ChangeClass,
        path: String,
        old: Option<&Value>,
        new: Option<&Value>,
    ) {
        let kind = match (old, new) {
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Removed,
            (Some(a), Some(b)) if a != b => ChangeKind::Modified,
            _ => return,
        };
        self.push(class, kind, path, old, new);
    }
}

fn diff_card(source: &ToolCard, target: &ToolCard, out: &mut Vec<ChangeRecord>) {
    let mut c = CardChanges { target, out };

    if source.tool_id != target.tool_id {
        c.push(
            ChangeClass::Tool,
            ChangeKind::Renamed,
            "tool_id".to_owned(),
            Some(&Value::String(source.tool_id.clone())),
            Some(&Value::String(target.tool_id.clone())),
        );
    }
    if source.name != target.name {
        c.push(
            ChangeClass::Tool,
            ChangeKind::Renamed,
            "name".to_owned(),
            Some(&Value::String(source.name.clone())),
            Some(&Value::String(target.name.clone())),
        );
    }

    diff_input_schema(&source.input_schema, &target.input_schema, &mut c);

    c.push_optional(
        ChangeClass::Parameter,
        "output_schema".to_owned(),
        source.output_schema.as_ref().filter(|v| !v.is_null()),
        target.output_schema.as_ref().filter(|v| !v.is_null()),
    );

    if source.description != target.description {
        c.push(
            ChangeClass::Description,
            ChangeKind::Modified,
            "description".to_owned(),
            Some(&Value::String(source.description.clone())),
            Some(&Value::String(target.description.clone())),
        );
    }
    if source.usage_rules != target.usage_rules {
        c.push(
            ChangeClass::Description,
            ChangeKind::Modified,
            "usage_rules".to_owned(),
            Some(&serde_json::to_value(&source.usage_rules).unwrap_or(Value::Null)),
            Some(&serde_json::to_value(&target.usage_rules).unwrap_or(Value::Null)),
        );
    }
}

fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

/// Required names as a sorted, de-duplicated array so reordering is not a change.
fn required_set(schema: &Value) -> Value {
    let names: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Value::Array(
        names
            .into_iter()
            .map(|n| Value::String(n.to_owned()))
            .collect(),
    )
}

fn diff_input_schema(source: &Value, target: &Value, c: &mut CardChanges<'_>) {
    if source == target {
        return;
    }
    let (Some(s_obj), Some(t_obj)) = (source.as_object(), target.as_object()) else {
        c.push_optional(
            ChangeClass::Parameter,
            "input_schema".to_owned(),
            Some(source),
            Some(target),
        );
        return;
    };

    // Properties (the parameters).
    let raw_s = s_obj.get("properties");
    let raw_t = t_obj.get("properties");
    let both_objects = raw_s.is_none_or(Value::is_object) && raw_t.is_none_or(Value::is_object);
    if both_objects {
        let empty = Map::new();
        let ps = properties(source).unwrap_or(&empty);
        let pt = properties(target).unwrap_or(&empty);
        let names: BTreeSet<&String> = ps.keys().chain(pt.keys()).collect();
        for name in names {
            let path = format!("input_schema.properties.{}", escape_path_segment(name));
            c.push_optional(ChangeClass::Parameter, path, ps.get(name), pt.get(name));
        }
    } else {
        c.push_optional(
            ChangeClass::Parameter,
            "input_schema.properties".to_owned(),
            raw_s,
            raw_t,
        );
    }

    // Required-ness.
    let (req_s, req_t) = (required_set(source), required_set(target));
    if req_s != req_t {
        c.push(
            ChangeClass::Parameter,
            ChangeKind::Modified,
            "input_schema.required".to_owned(),
            Some(&req_s),
            Some(&req_t),
        );
    }

    // Any other top-level keyword (type, additionalProperties, ...).
    let others: BTreeSet<&String> = s_obj
        .keys()
        .chain(t_obj.keys())
        .filter(|k| k.as_str() != "properties" && k.as_str() != "required")
        .collect();
    for key in others {
        let path = format!("input_schema.{}", escape_path_segment(key));
        c.push_optional(ChangeClass::Parameter, path, s_obj.get(key), t_obj.get(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_escaping_round_trips() {
        let raw = "a.b~c";
        let escaped = escape_path_segment(raw);
        assert_eq!(escaped, "a~1b~0c");
        assert_eq!(unescape_path_segment(&escaped), raw);
    }

    #[test]
    fn field_hash_ignores_key_order() {
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(field_hash(&a), field_hash(&b));
    }
}
