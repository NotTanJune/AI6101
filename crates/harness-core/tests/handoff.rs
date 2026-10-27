//! Offline handoff contract and stream-consistency checks.

use std::path::{Path, PathBuf};

use harness_core::handoff::{HandoffError, check_handoff};
use harness_core::{EventSource, Metric};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/contracts")
        .join(name)
}

fn metric(run_id: &str, source: &str, seq: u64) -> Value {
    json!({
        "schema_version": "run-event/1", "run_id": run_id,
        "seq": seq, "ts": "2026-10-25T10:00:00Z", "source": source,
        "strategy": null, "event_type": "metric_score", "metric": "tf",
        "task_id": null, "value": 4.35
    })
}

fn canary(run_id: &str, source: &str, seq: u64, event_refs: &[u64]) -> Value {
    json!({
        "schema_version": "run-event/1", "run_id": run_id,
        "seq": seq, "ts": "2026-10-25T10:00:01Z", "source": source,
        "strategy": "selective_patch", "event_type": "canary_result",
        "status": "pass", "tool_id": "get_forecast", "probe_id": "p1",
        "is_control": false, "expected": "accepted", "actual": "accepted",
        "event_refs": event_refs
    })
}

fn write_events(dir: &Path, events: &[Value]) -> PathBuf {
    let path = dir.join("events.jsonl");
    let mut text = events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    if !events.is_empty() {
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write test log");
    path
}

#[test]
fn valid_contract_fixtures_report_coverage_without_requiring_success() {
    let summary = check_handoff(fixture("tool-kb.valid.json"), fixture("events.valid.jsonl"))
        .expect("valid fixtures");
    assert_eq!(summary.tool_kb_schema_version, "tool-kb/1");
    assert_eq!(summary.run_event_schema_version, "run-event/1");
    assert_eq!(summary.events, 29);
    assert_eq!(summary.runs, 3);
    assert_eq!(summary.streams, 6);
    assert_eq!(summary.source_event_counts[&EventSource::Rust], 12);
    assert_eq!(summary.source_event_counts[&EventSource::Python], 17);
    assert_eq!(summary.metric_event_counts.len(), 5);
    assert_eq!(summary.metric_event_counts[&Metric::Tf], 1);
}

#[test]
fn sequences_are_scoped_to_run_and_source_and_need_not_be_contiguous() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = write_events(
        dir.path(),
        &[
            metric("r1", "rust", 5),
            metric("r1", "python", 0),
            metric("r2", "rust", 0),
            metric("r1", "rust", 9),
        ],
    );
    let summary =
        check_handoff(fixture("tool-kb.valid.json"), events).expect("independent streams");
    assert_eq!(summary.runs, 2);
    assert_eq!(summary.streams, 3);
    assert_eq!(summary.events, 4);
}

#[test]
fn repeated_or_decreasing_sequences_are_rejected_in_their_own_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    for seq in [8, 7] {
        let events = write_events(
            dir.path(),
            &[
                metric("r1", "rust", 8),
                metric("r1", "python", 2),
                metric("r1", "rust", seq),
            ],
        );
        assert!(matches!(
            check_handoff(fixture("tool-kb.valid.json"), events),
            Err(HandoffError::Sequence {
                line: 3,
                previous: 8,
                ..
            })
        ));
    }
}

#[test]
fn canary_references_must_exist_earlier_in_the_same_run_and_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let valid = write_events(
        dir.path(),
        &[metric("r", "rust", 4), canary("r", "rust", 6, &[4])],
    );
    check_handoff(fixture("tool-kb.valid.json"), valid).expect("earlier reference");

    for earlier in [
        metric("r", "python", 4),
        metric("another-run", "rust", 4),
        metric("r", "rust", 3),
    ] {
        let events = write_events(dir.path(), &[earlier, canary("r", "rust", 6, &[4])]);
        assert!(matches!(
            check_handoff(fixture("tool-kb.valid.json"), events),
            Err(HandoffError::MissingReference {
                line: 2,
                reference: 4
            })
        ));
    }
    for reference in [6, 7] {
        let events = write_events(
            dir.path(),
            &[
                metric("r", "rust", 4),
                canary("r", "rust", 6, &[reference]),
                metric("r", "rust", 7),
            ],
        );
        assert!(matches!(
            check_handoff(fixture("tool-kb.valid.json"), events),
            Err(HandoffError::MissingReference { line: 2, .. })
        ));
    }
}

#[test]
fn malformed_kb_and_missing_inputs_are_rejected() {
    assert!(matches!(
        check_handoff(
            fixture("tool-kb.invalid-missing-field.json"),
            fixture("events.valid.jsonl")
        ),
        Err(HandoffError::ToolKb { .. })
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("absent.json");
    assert!(matches!(
        check_handoff(&absent, fixture("events.valid.jsonl")),
        Err(HandoffError::ToolKb {
            reason: "cannot read file"
        })
    ));
    assert!(matches!(
        check_handoff(fixture("tool-kb.valid.json"), &absent),
        Err(HandoffError::ReadEvents)
    ));
}

#[test]
fn empty_logs_and_malformed_lines_are_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_events(dir.path(), &[]);
    assert!(matches!(
        check_handoff(fixture("tool-kb.valid.json"), &path),
        Err(HandoffError::EmptyEvents)
    ));
    std::fs::write(&path, format!("{}\nnot json\n", metric("r", "python", 0)))
        .expect("write malformed JSONL");
    assert!(matches!(
        check_handoff(fixture("tool-kb.valid.json"), &path),
        Err(HandoffError::InvalidEvent {
            line: 2,
            reason: "invalid JSON"
        })
    ));
}

#[test]
fn unsupported_metric_is_rejected_without_exposing_payloads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut event = metric("r", "python", 0);
    event["metric"] = json!("not_a_metric");
    let path = write_events(dir.path(), &[event]);
    assert!(matches!(
        check_handoff(fixture("tool-kb.valid.json"), &path),
        Err(HandoffError::InvalidEvent {
            line: 1,
            reason: "schema validation failed"
        })
    ));
    std::fs::write(&path, "{\"schema_version\":\"not-a-real-secret-123456\"}\n")
        .expect("write unknown schema");
    let error = check_handoff(fixture("tool-kb.valid.json"), &path)
        .expect_err("unsupported schema")
        .to_string();
    assert!(error.contains("line 1"));
    assert!(!error.contains("not-a-real-secret"));
}
