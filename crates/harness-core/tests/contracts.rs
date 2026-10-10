//! Contract tests: the shared fixtures must pass when intact and fail closed when damaged.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use harness_core::schema::{
    RUN_EVENT_SCHEMA_JSON, TOOL_KB_SCHEMA_JSON, validate_run_event, validate_tool_kb,
};
use harness_core::{
    CanaryStatus, CostStatus, EventBody, EventError, EventSource, FailureCategory, Metric, Phase,
    RunEvent, ToolKb, VerifierOutcome, canonical_json, card_hash, parse_event_line, read_events,
    read_events_partial,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/contracts")
        .join(name)
}

fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixture(name)).expect("fixture readable")
}

fn fixture_lines(name: &str) -> Vec<String> {
    fixture_text(name).lines().map(str::to_owned).collect()
}

fn fixture_events() -> Vec<RunEvent> {
    read_events(fixture("events.valid.jsonl")).expect("valid fixture parses")
}

fn schema_defs(text: &str) -> Value {
    serde_json::from_str::<Value>(text).expect("schema is JSON")["$defs"].clone()
}

fn required_of(def: &Value) -> Vec<String> {
    def["required"]
        .as_array()
        .expect("required list")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Tool KB
// ---------------------------------------------------------------------------------------------

#[test]
fn tool_kb_fixture_passes_schema_and_typed_load() {
    let value: Value = serde_json::from_str(&fixture_text("tool-kb.valid.json")).unwrap();
    assert_eq!(validate_tool_kb(&value), Ok(()));
    let kb = ToolKb::read(fixture("tool-kb.valid.json")).expect("typed load");
    assert_eq!(kb.cards.len(), 3);
    // Typed round trip keeps the document identical.
    assert_eq!(serde_json::to_value(&kb).unwrap(), value);
}

#[test]
fn tool_kb_invalid_fixture_names_missing_field() {
    let value: Value =
        serde_json::from_str(&fixture_text("tool-kb.invalid-missing-field.json")).unwrap();
    let errors = validate_tool_kb(&value).expect_err("must fail");
    assert!(
        errors.iter().any(|e| e.contains("lineage_id")),
        "errors should name the missing field: {errors:?}"
    );
    assert!(ToolKb::read(fixture("tool-kb.invalid-missing-field.json")).is_err());
}

#[test]
fn tool_kb_each_required_field_is_enforced() {
    let valid: Value = serde_json::from_str(&fixture_text("tool-kb.valid.json")).unwrap();
    let schema: Value = serde_json::from_str(TOOL_KB_SCHEMA_JSON).unwrap();
    let defs = schema_defs(TOOL_KB_SCHEMA_JSON);

    let mut checked = 0;
    for field in required_of(&schema) {
        let mut doc = valid.clone();
        doc.as_object_mut().unwrap().remove(&field);
        assert!(validate_tool_kb(&doc).is_err(), "root field {field}");
        checked += 1;
    }
    for field in required_of(&defs["card"]) {
        let mut doc = valid.clone();
        doc["cards"][0].as_object_mut().unwrap().remove(&field);
        assert!(validate_tool_kb(&doc).is_err(), "card field {field}");
        checked += 1;
    }
    for field in required_of(&defs["provenance"]) {
        let mut doc = valid.clone();
        doc["cards"][0]["provenance"]
            .as_object_mut()
            .unwrap()
            .remove(&field);
        assert!(validate_tool_kb(&doc).is_err(), "provenance field {field}");
        checked += 1;
    }
    assert_eq!(checked, 5 + 10 + 3);
}

#[test]
fn tool_kb_rejects_unknown_fields_wrong_types_and_versions() {
    let valid: Value = serde_json::from_str(&fixture_text("tool-kb.valid.json")).unwrap();

    let mut extra = valid.clone();
    extra["cards"][0]["surprise"] = json!(1);
    assert!(validate_tool_kb(&extra).is_err());

    let mut wrong = valid.clone();
    wrong["cards"][0]["usage_rules"] = json!("not a list");
    assert!(validate_tool_kb(&wrong).is_err());

    let mut version = valid.clone();
    version["schema_version"] = json!("tool-kb/2");
    let errors = validate_tool_kb(&version).expect_err("unsupported version");
    assert!(
        errors[0].contains("unsupported schema_version"),
        "{errors:?}"
    );
}

#[test]
fn card_hash_is_stable_across_reload_and_key_order() {
    let kb = ToolKb::read(fixture("tool-kb.valid.json")).unwrap();
    let again = ToolKb::read(fixture("tool-kb.valid.json")).unwrap();
    assert_eq!(card_hash(&kb.cards[0]), card_hash(&again.cards[0]));

    // Reordering keys inside a schema must not change the hash.
    let text = fixture_text("tool-kb.valid.json").replace(
        r#""type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]"#,
        r#""required": ["path"],
        "properties": { "path": { "type": "string" } },
        "type": "object""#,
    );
    assert_ne!(
        text,
        fixture_text("tool-kb.valid.json"),
        "replacement applied"
    );
    let reordered = ToolKb::from_json_str(&text).unwrap();
    assert_eq!(card_hash(&kb.cards[1]), card_hash(&reordered.cards[1]));
    assert_eq!(canonical_json(&kb), canonical_json(&reordered));
}

// ---------------------------------------------------------------------------------------------
// Run events
// ---------------------------------------------------------------------------------------------

#[test]
fn valid_events_fixture_parses_line_by_line() {
    let lines = fixture_lines("events.valid.jsonl");
    assert_eq!(lines.len(), 29);
    for (i, line) in lines.iter().enumerate() {
        parse_event_line(i + 1, line).unwrap_or_else(|e| panic!("line {}: {e}", i + 1));
    }
    assert_eq!(fixture_events().len(), 29);
}

#[test]
fn valid_events_round_trip_losslessly() {
    for (i, line) in fixture_lines("events.valid.jsonl").iter().enumerate() {
        let raw: Value = serde_json::from_str(line).unwrap();
        let event = parse_event_line(i + 1, line).unwrap();
        assert_eq!(event.to_value().unwrap(), raw, "line {}", i + 1);
    }
}

#[test]
fn fixture_covers_every_event_type_and_enum_value() {
    let events = fixture_events();
    let types: BTreeSet<_> = events.iter().map(|e| e.body.event_type()).collect();
    assert_eq!(types.len(), 12, "{types:?}");

    let sources: BTreeSet<_> = events.iter().map(|e| e.source).collect();
    assert_eq!(
        sources,
        BTreeSet::from([EventSource::Rust, EventSource::Python])
    );

    let mut canary = BTreeSet::new();
    let mut verdicts = BTreeSet::new();
    let mut metrics = BTreeSet::new();
    let mut failures = BTreeSet::new();
    let mut costs = BTreeSet::new();
    let mut phases = BTreeSet::new();
    for e in &events {
        match &e.body {
            EventBody::CanaryResult(c) => {
                canary.insert(c.status);
            }
            EventBody::VerifierVerdict(v) => {
                verdicts.insert(v.outcome);
            }
            EventBody::MetricScore(m) => {
                metrics.insert(m.metric);
            }
            EventBody::FailureCategory(f) => {
                failures.insert(f.category);
            }
            EventBody::ModelCall(m) => {
                costs.insert(m.cost_status);
                phases.insert(m.phase);
            }
            _ => {}
        }
    }
    assert_eq!(
        canary,
        BTreeSet::from([
            CanaryStatus::Pass,
            CanaryStatus::Fail,
            CanaryStatus::Inconclusive,
            CanaryStatus::NotApplicable
        ])
    );
    assert_eq!(verdicts.len(), 3);
    assert!(verdicts.contains(&VerifierOutcome::Inconclusive));
    assert_eq!(
        metrics,
        BTreeSet::from([
            Metric::Tf,
            Metric::Pe,
            Metric::Ecs,
            Metric::Recovery,
            Metric::Retention
        ])
    );
    assert_eq!(
        failures,
        BTreeSet::from([FailureCategory::UnderUpdate, FailureCategory::OverUpdate])
    );
    assert_eq!(
        costs,
        BTreeSet::from([
            CostStatus::Priced,
            CostStatus::Unknown,
            CostStatus::Mock,
            CostStatus::Local
        ])
    );
    assert!(phases.contains(&Phase::Adaptation) && phases.contains(&Phase::Verifier));
}

#[test]
fn seq_is_monotonic_per_run_and_source() {
    let mut last: std::collections::BTreeMap<(String, EventSource), u64> = Default::default();
    for e in fixture_events() {
        let key = (e.run_id.clone(), e.source);
        if let Some(prev) = last.insert(key, e.seq) {
            assert_eq!(e.seq, prev + 1, "run {} source {:?}", e.run_id, e.source);
        } else {
            assert_eq!(e.seq, 0);
        }
    }
}

#[test]
fn removing_any_required_field_fails_every_event() {
    let defs = schema_defs(RUN_EVENT_SCHEMA_JSON);
    let mut removals = 0;
    for (i, line) in fixture_lines("events.valid.jsonl").iter().enumerate() {
        let value: Value = serde_json::from_str(line).unwrap();
        let event_type = value["event_type"].as_str().unwrap().to_owned();
        for field in required_of(&defs[&event_type]) {
            let mut damaged = value.clone();
            damaged.as_object_mut().unwrap().remove(&field);
            assert!(
                validate_run_event(&damaged).is_err(),
                "line {}: removing {field} from {event_type} must fail",
                i + 1
            );
            let text = serde_json::to_string(&damaged).unwrap();
            let err = parse_event_line(i + 1, &text).expect_err("parse must fail");
            assert_eq!(err.line(), Some(i + 1));
            removals += 1;
        }
    }
    assert!(removals > 200, "loop actually ran: {removals}");
}

#[test]
fn removing_nested_change_record_fields_fails() {
    let defs = schema_defs(RUN_EVENT_SCHEMA_JSON);
    let line = &fixture_lines("events.valid.jsonl")[1];
    let value: Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["event_type"], "adaptation_diff");
    for field in required_of(&defs["change_record"]) {
        let mut damaged = value.clone();
        damaged["changes"][0]
            .as_object_mut()
            .unwrap()
            .remove(&field);
        assert!(
            validate_run_event(&damaged).is_err(),
            "change_record.{field}"
        );
    }
}

#[test]
fn extra_field_on_every_event_fails() {
    for (i, line) in fixture_lines("events.valid.jsonl").iter().enumerate() {
        let mut value: Value = serde_json::from_str(line).unwrap();
        value["unexpected"] = json!(true);
        assert!(validate_run_event(&value).is_err(), "line {}", i + 1);
    }
}

#[test]
fn integer_metric_value_is_accepted() {
    let line = r#"{"schema_version":"run-event/1","run_id":"r","seq":0,"ts":"2026-09-01T10:00:00Z","source":"python","strategy":null,"event_type":"metric_score","metric":"tf","task_id":null,"value":1}"#;
    let event = parse_event_line(1, line).unwrap();
    assert!(matches!(event.body, EventBody::MetricScore(ref m) if m.value == 1.0));
}

#[test]
fn malformed_fixture_lines_each_fail_for_their_own_reason() {
    let lines = fixture_lines("events.malformed.jsonl");
    assert_eq!(lines.len(), 6);
    let errors: Vec<EventError> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| parse_event_line(i + 1, l).expect_err("must fail"))
        .collect();

    assert!(
        matches!(errors[0], EventError::BadJson { line: 1, .. }),
        "{}",
        errors[0]
    );
    assert!(
        matches!(&errors[1], EventError::Schema { line: 2, errors } if errors.iter().any(|e| e.contains("fatal"))),
        "{}",
        errors[1]
    );
    assert!(
        matches!(&errors[2], EventError::Schema { line: 3, errors } if errors.iter().any(|e| e.contains("unknown event_type"))),
        "{}",
        errors[2]
    );
    assert!(
        matches!(&errors[3], EventError::UnsupportedSchemaVersion { line: 4, found } if found == "\"run-event/2\""),
        "{}",
        errors[3]
    );
    assert!(
        matches!(&errors[4], EventError::Schema { line: 5, errors } if errors.iter().any(|e| e.contains("surprise"))),
        "{}",
        errors[4]
    );
    assert!(
        matches!(&errors[5], EventError::Schema { line: 6, errors } if errors.iter().any(|e| e.contains("/seq"))),
        "{}",
        errors[5]
    );
    for (i, e) in errors.iter().enumerate() {
        assert!(
            e.to_string().starts_with(&format!("line {}:", i + 1)),
            "{e}"
        );
    }
}

#[test]
fn unsupported_schema_version_fails_closed_naming_the_line() {
    let good = fixture_lines("events.valid.jsonl")[0].clone();
    let bad = good.replace("run-event/1", "run-event/9");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    fs::write(&path, format!("{good}\n{good}\n{bad}\n")).unwrap();

    let err = read_events(&path).expect_err("must fail closed");
    assert!(
        matches!(err, EventError::UnsupportedSchemaVersion { line: 3, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("line 3"), "{err}");
    assert!(err.to_string().contains("run-event/9"), "{err}");

    // A non-string version is unsupported too, not silently accepted.
    let numeric = good.replace("\"run-event/1\"", "1");
    assert!(matches!(
        parse_event_line(1, &numeric),
        Err(EventError::UnsupportedSchemaVersion { line: 1, .. })
    ));
}

#[test]
fn blank_lines_are_rejected_with_their_line_number() {
    let good = fixture_lines("events.valid.jsonl")[0].clone();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    fs::write(&path, format!("{good}\n\n{good}\n")).unwrap();
    let err = read_events(&path).expect_err("blank line");
    assert!(matches!(err, EventError::BlankLine { line: 2 }), "{err}");
    assert!(matches!(
        parse_event_line(7, "   "),
        Err(EventError::BlankLine { line: 7 })
    ));
}

#[test]
fn partial_log_returns_valid_prefix_and_first_error() {
    let lines = fixture_lines("events.valid.jsonl");
    let truncated = &lines[2][..lines[2].len() / 2];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.jsonl");
    fs::write(&path, format!("{}\n{}\n{truncated}", lines[0], lines[1])).unwrap();

    let (events, err) = read_events_partial(&path);
    assert_eq!(events.len(), 2);
    let err = err.expect("truncated third line is reported");
    assert_eq!(err.line(), Some(3));

    let (all, none) = read_events_partial(fixture("events.valid.jsonl"));
    assert_eq!(all.len(), 29);
    assert!(none.is_none());

    let (empty, missing) = read_events_partial(dir.path().join("missing.jsonl"));
    assert!(empty.is_empty());
    assert!(matches!(missing, Some(EventError::Io { .. })));

    let whole = read_events(fixture("events.malformed.jsonl")).expect_err("malformed");
    assert_eq!(whole.line(), Some(1));
}
