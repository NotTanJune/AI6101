//! Embedded JSON Schemas (draft 2020-12) for the two Rust/Python interchange contracts and
//! validators that return readable error lists.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::{Value, json};

/// Tool KB schema document, embedded at compile time.
pub const TOOL_KB_SCHEMA_JSON: &str = include_str!("../../../schemas/tool-kb.schema.json");
/// Run-event schema document, embedded at compile time.
pub const RUN_EVENT_SCHEMA_JSON: &str = include_str!("../../../schemas/run-event.schema.json");

/// `event_type` values that have a schema variant.
pub const EVENT_TYPES: [&str; 12] = [
    "config",
    "adaptation_diff",
    "adaptation_outcome",
    "model_call",
    "mcp_action",
    "canary_result",
    "verifier_probe",
    "verifier_verdict",
    "metric_score",
    "failure_category",
    "error",
    "run_complete",
];

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

struct RunEventValidators {
    /// The complete `oneOf` schema, as published.
    root: Validator,
    /// One validator per `event_type`, for precise error messages.
    variants: BTreeMap<&'static str, Validator>,
}

fn compile(schema: &Value) -> Result<Validator, String> {
    jsonschema::validator_for(schema).map_err(|e| e.to_string())
}

fn tool_kb_validator() -> Result<&'static Validator, String> {
    static V: OnceLock<Result<Validator, String>> = OnceLock::new();
    V.get_or_init(|| {
        let schema: Value = serde_json::from_str(TOOL_KB_SCHEMA_JSON).map_err(|e| e.to_string())?;
        compile(&schema)
    })
    .as_ref()
    .map_err(|e| format!("internal error: tool-kb schema did not compile: {e}"))
}

fn run_event_validators() -> Result<&'static RunEventValidators, String> {
    static V: OnceLock<Result<RunEventValidators, String>> = OnceLock::new();
    V.get_or_init(|| {
        let schema: Value =
            serde_json::from_str(RUN_EVENT_SCHEMA_JSON).map_err(|e| e.to_string())?;
        let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
        let mut variants = BTreeMap::new();
        for name in EVENT_TYPES {
            let wrapper = json!({
                "$schema": DRAFT_2020_12,
                "$ref": format!("#/$defs/{name}"),
                "$defs": defs,
            });
            variants.insert(name, compile(&wrapper)?);
        }
        Ok(RunEventValidators {
            root: compile(&schema)?,
            variants,
        })
    })
    .as_ref()
    .map_err(|e| format!("internal error: run-event schema did not compile: {e}"))
}

fn collect_errors(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|e| {
            let path = e.instance_path().to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .collect()
}

/// Validate a Tool KB document. Returns every violation, or `Ok(())`.
pub fn validate_tool_kb(value: &Value) -> Result<(), Vec<String>> {
    let validator = tool_kb_validator().map_err(|e| vec![e])?;
    if let Some(found) = version_mismatch(value, crate::model::TOOL_KB_SCHEMA_VERSION) {
        return Err(vec![found]);
    }
    let errors = collect_errors(validator, value);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate one run event. Returns every violation, or `Ok(())`.
///
/// The variant is chosen by `event_type` so messages point at the offending field instead of
/// the generic `oneOf` failure; the full published `oneOf` schema is also applied.
pub fn validate_run_event(value: &Value) -> Result<(), Vec<String>> {
    let validators = run_event_validators().map_err(|e| vec![e])?;
    if !value.is_object() {
        return Err(vec!["event must be a JSON object".to_owned()]);
    }
    if let Some(found) = version_mismatch(value, crate::events::RUN_EVENT_SCHEMA_VERSION) {
        return Err(vec![found]);
    }
    let variant = match value.get("event_type") {
        None => return Err(vec!["missing required field \"event_type\"".to_owned()]),
        Some(Value::String(name)) => match validators.variants.get(name.as_str()) {
            Some(v) => v,
            None => {
                return Err(vec![format!(
                    "unknown event_type {name:?} (expected one of: {})",
                    EVENT_TYPES.join(", ")
                )]);
            }
        },
        Some(other) => return Err(vec![format!("event_type must be a string, got {other}")]),
    };
    let mut errors = collect_errors(variant, value);
    if errors.is_empty() && !validators.root.is_valid(value) {
        errors.push("event does not match the run-event schema".to_owned());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Error text when `schema_version` is present but not the supported value.
fn version_mismatch(value: &Value, supported: &str) -> Option<String> {
    match value.get("schema_version") {
        Some(Value::String(found)) if found == supported => None,
        Some(other) => Some(format!(
            "unsupported schema_version {other} (supported: \"{supported}\")"
        )),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_compile() {
        assert!(tool_kb_validator().is_ok());
        assert!(run_event_validators().is_ok());
    }

    #[test]
    fn every_defined_variant_is_listed() {
        let schema: Value = serde_json::from_str(RUN_EVENT_SCHEMA_JSON).unwrap();
        let one_of = schema["oneOf"].as_array().unwrap().len();
        assert_eq!(one_of, EVENT_TYPES.len());
        for name in EVENT_TYPES {
            assert_eq!(
                schema["$defs"][name]["properties"]["event_type"]["const"], name,
                "{name}"
            );
        }
    }

    #[test]
    fn schema_ids_carry_version() {
        for text in [TOOL_KB_SCHEMA_JSON, RUN_EVENT_SCHEMA_JSON] {
            let schema: Value = serde_json::from_str(text).unwrap();
            assert!(schema["$id"].as_str().unwrap().ends_with(":1"));
            assert_eq!(schema["$schema"], DRAFT_2020_12);
        }
    }
}
