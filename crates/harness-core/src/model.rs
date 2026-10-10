//! Tool KB types, canonical JSON and content hashing.
//!
//! Canonical JSON is compact JSON with object keys sorted at every level. It relies on
//! `serde_json` being built WITHOUT the `preserve_order` feature, which the unit tests
//! below guard.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{HarnessError, Result};
use crate::schema;

/// Schema version string carried by every Tool KB document.
pub const TOOL_KB_SCHEMA_VERSION: &str = "tool-kb/1";

/// Server version stage used by the experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStage {
    /// Earliest server version.
    Early,
    /// Intermediate server version.
    Middle,
    /// Latest server version.
    Late,
}

/// Level at which a tool change happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClass {
    /// Whole tool added, removed or renamed.
    Tool,
    /// Parameter or schema change.
    Parameter,
    /// Description or usage-rule change.
    Description,
}

/// Where a card came from in the supplied catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Release the card was read from.
    pub release_id: String,
    /// Source file, relative to the catalog root.
    pub source_file: String,
    /// Optional pointer (for example a JSON pointer) inside the source file.
    pub source_pointer: Option<String>,
}

/// One tool description card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCard {
    /// Stable id for a tool across versions (renames keep it).
    pub lineage_id: String,
    /// Server the tool belongs to.
    pub server_id: String,
    /// Source-native tool id.
    pub tool_id: String,
    /// Display name.
    pub name: String,
    /// Server version this card describes.
    pub server_version: String,
    /// Natural-language description.
    pub description: String,
    /// JSON Schema of the tool input.
    pub input_schema: Value,
    /// JSON Schema of the tool output, when the source exposes one.
    pub output_schema: Option<Value>,
    /// Usage rules attached to the tool.
    pub usage_rules: Vec<String>,
    /// Source provenance.
    pub provenance: Provenance,
}

/// A Tool KB document (`tool-kb/1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolKb {
    /// Must equal [`TOOL_KB_SCHEMA_VERSION`].
    pub schema_version: String,
    /// Knowledge base id.
    pub kb_id: String,
    /// Benchmark release id.
    pub release_id: String,
    /// Server version label this KB describes.
    pub server_version: String,
    /// Tool cards.
    pub cards: Vec<ToolCard>,
}

impl ToolKb {
    /// Copy of the KB with cards ordered by `(server_id, lineage_id)`.
    pub fn sorted(&self) -> ToolKb {
        let mut kb = self.clone();
        kb.cards.sort_by(|a, b| {
            (&a.server_id, &a.lineage_id, &a.tool_id).cmp(&(
                &b.server_id,
                &b.lineage_id,
                &b.tool_id,
            ))
        });
        kb
    }

    /// Canonical serialization of the sorted KB, as bytes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_json(&self.sorted()).into_bytes()
    }

    /// Parse and validate a Tool KB from JSON text (schema first, then typed).
    pub fn from_json_str(text: &str) -> Result<ToolKb> {
        let value: Value = serde_json::from_str(text)?;
        schema::validate_tool_kb(&value).map_err(|errors| HarnessError::Schema {
            what: "tool KB",
            errors,
        })?;
        Ok(serde_json::from_value(value)?)
    }

    /// Read and validate a Tool KB file.
    pub fn read(path: impl AsRef<Path>) -> Result<ToolKb> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| HarnessError::io(path, e))?;
        Self::from_json_str(&text)
    }
}

/// Canonical JSON of `value`, or an error when it cannot be serialized.
pub fn try_canonical_json<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    // Going through `Value` sorts keys at every level (BTreeMap-backed objects).
    let value = serde_json::to_value(value)?;
    serde_json::to_string(&value)
}

/// Compact JSON with sorted keys.
///
/// Serialization cannot fail for the harness types (string-keyed maps only); for an
/// arbitrary `Serialize` that does fail this returns the literal `null`. Use
/// [`try_canonical_json`] when failure must be observed.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> String {
    try_canonical_json(value).unwrap_or_else(|_| "null".to_owned())
}

/// SHA-256 (lowercase hex) of the card's canonical JSON.
pub fn card_hash(card: &ToolCard) -> String {
    hex::encode(Sha256::digest(canonical_json(card).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(server: &str, lineage: &str) -> ToolCard {
        ToolCard {
            lineage_id: lineage.into(),
            server_id: server.into(),
            tool_id: format!("{lineage}_tool"),
            name: lineage.into(),
            server_version: "early".into(),
            description: "d".into(),
            input_schema: json!({"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "integer"}}}),
            output_schema: None,
            usage_rules: vec!["r".into()],
            provenance: Provenance {
                release_id: "rel".into(),
                source_file: "x.json".into(),
                source_pointer: None,
            },
        }
    }

    #[test]
    fn serde_json_keeps_sorted_keys() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":{"z":1,"y":2}}"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":{"y":2,"z":1},"b":1}"#);
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":[{"q":1,"p":2}]}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":[{"p":2,"q":1}],"x":1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn card_hash_is_deterministic_and_content_sensitive() {
        let c = card("s", "one");
        assert_eq!(card_hash(&c), card_hash(&c.clone()));
        assert_eq!(card_hash(&c).len(), 64);
        let mut changed = c.clone();
        changed.description = "other".into();
        assert_ne!(card_hash(&c), card_hash(&changed));
    }

    #[test]
    fn card_hash_ignores_schema_key_order() {
        let mut a = card("s", "one");
        let mut b = card("s", "one");
        a.input_schema = serde_json::from_str(r#"{"type":"object","required":["a"]}"#).unwrap();
        b.input_schema = serde_json::from_str(r#"{"required":["a"],"type":"object"}"#).unwrap();
        assert_eq!(card_hash(&a), card_hash(&b));
    }

    #[test]
    fn sorted_orders_by_server_then_lineage() {
        let kb = ToolKb {
            schema_version: TOOL_KB_SCHEMA_VERSION.into(),
            kb_id: "kb".into(),
            release_id: "rel".into(),
            server_version: "early".into(),
            cards: vec![card("b", "z"), card("a", "y"), card("a", "x")],
        };
        let order: Vec<_> = kb
            .sorted()
            .cards
            .iter()
            .map(|c| (c.server_id.clone(), c.lineage_id.clone()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a".into(), "x".into()),
                ("a".into(), "y".into()),
                ("b".into(), "z".into())
            ]
        );
        let mut shuffled = kb.clone();
        shuffled.cards.reverse();
        assert_eq!(kb.canonical_bytes(), shuffled.canonical_bytes());
    }

    #[test]
    fn stage_and_class_serialize_snake_case() {
        assert_eq!(canonical_json(&VersionStage::Middle), "\"middle\"");
        assert_eq!(canonical_json(&ChangeClass::Description), "\"description\"");
    }
}
