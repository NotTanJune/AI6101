//! Transcript lines against the published schema, and secret redaction.

mod common;

use common::{DUMMY_KEY, HarnessOpts, harness, repo_root, run_turn};
use harness_agent::agent::StopReason;
use harness_agent::approval::Decision;
use harness_agent::model::mock::Scripted;
use harness_agent::transcript::{
    TRANSCRIPT_FILE, TRANSCRIPT_SCHEMA_JSON, TRANSCRIPT_SCHEMA_VERSION, TranscriptEvent,
    TranscriptWriter,
};
use harness_core::redact::Redactor;
use jsonschema::Validator;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn validator() -> Validator {
    let schema: Value = serde_json::from_str(TRANSCRIPT_SCHEMA_JSON).expect("schema is JSON");
    jsonschema::validator_for(&schema).expect("schema compiles")
}

fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("transcript readable")
        .lines()
        .map(|l| serde_json::from_str(l).expect("every line is JSON"))
        .collect()
}

#[test]
fn transcript_embedded_schema_matches_the_file_in_schemas_dir() {
    let on_disk =
        std::fs::read_to_string(repo_root().join("schemas/agent-session.schema.json")).unwrap();
    assert_eq!(TRANSCRIPT_SCHEMA_JSON, on_disk);
    assert_eq!(TRANSCRIPT_SCHEMA_VERSION, "agent-session/1");
}

#[tokio::test]
async fn transcript_lines_of_a_real_turn_validate_and_hold_no_secret() {
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("reading", &[("read_file", json!({"path": "notes.txt"}))]),
        Scripted::tool_calls(
            "",
            &[("write_file", json!({"path": "out.txt", "content": "copy"}))],
        ),
        Scripted::text(format!("done, the key was {DUMMY_KEY}")),
    ]);
    opts.transcript = true;
    opts.redact_key = true;
    let mut h = harness(opts);
    h.agent.record_session_start();
    std::fs::write(
        h.root.join("notes.txt"),
        format!("api_key={DUMMY_KEY}\nsecond line\n"),
    )
    .expect("write");

    let (reason, _) = run_turn(
        &mut h.agent,
        &format!("please read notes.txt, my key is {DUMMY_KEY}"),
        CancellationToken::new(),
        |_| Decision::Once,
    )
    .await;
    assert_eq!(reason, StopReason::Completed);

    let path = h.session_dir.join(TRANSCRIPT_FILE);
    let raw = std::fs::read_to_string(&path).expect("transcript exists");
    assert!(
        !raw.contains(DUMMY_KEY),
        "the API key must never be written"
    );
    assert!(raw.contains("[REDACTED]"));

    let validator = validator();
    let parsed = lines(&path);
    for (index, line) in parsed.iter().enumerate() {
        let errors: Vec<String> = validator.iter_errors(line).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "line {index} {line}: {errors:?}");
        assert_eq!(line["schema"], TRANSCRIPT_SCHEMA_VERSION);
    }
    let kinds: Vec<&str> = parsed.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec![
            "session_start",
            "user_message",
            "usage",
            "assistant_message",
            "tool_call",
            "tool_result",
            "usage",
            "assistant_message",
            "tool_call",
            "approval",
            "tool_result",
            "usage",
            "assistant_message",
            "turn_finished",
        ]
    );
    let finished = parsed.last().unwrap();
    assert_eq!(finished["reason"], "completed");
    let approval = parsed.iter().find(|l| l["kind"] == "approval").unwrap();
    assert_eq!(approval["decision"], "once");
    assert_eq!(approval["class"], "write");
    let usage = parsed.iter().find(|l| l["kind"] == "usage").unwrap();
    assert_eq!(usage["input_tokens"], 100);
    assert_eq!(usage["output_tokens"], 20);
}

#[test]
fn transcript_schema_rejects_malformed_lines() {
    let validator = validator();
    let good = json!({"schema": "agent-session/1", "ts": "2026-10-04T10:00:00.000Z",
                      "kind": "notice", "text": "hi"});
    assert!(validator.is_valid(&good));

    let mut wrong_version = good.clone();
    wrong_version["schema"] = json!("agent-session/2");
    assert!(!validator.is_valid(&wrong_version));

    let mut unknown_kind = good.clone();
    unknown_kind["kind"] = json!("mystery");
    assert!(!validator.is_valid(&unknown_kind));

    let mut extra = good.clone();
    extra["surprise"] = json!(1);
    assert!(!validator.is_valid(&extra));

    let mut bad_ts = good.clone();
    bad_ts["ts"] = json!("yesterday");
    assert!(!validator.is_valid(&bad_ts));

    let missing = json!({"schema": "agent-session/1", "ts": "2026-10-04T10:00:00Z",
                         "kind": "tool_result", "call_id": "c", "name": "n"});
    assert!(!validator.is_valid(&missing));
}

#[test]
fn transcript_writer_appends_redacts_nested_values_and_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir.path().join("nested/session");
    let redactor = Redactor::new(vec![DUMMY_KEY.to_owned()]);
    {
        let mut writer = TranscriptWriter::create(&session, redactor.clone()).expect("create");
        writer
            .record(&TranscriptEvent::ToolCall {
                call_id: "c1".into(),
                name: "mcp__x__y".into(),
                arguments: json!({"auth": {"token": "abcdef123456"}, "note": format!("k={DUMMY_KEY}")}),
            })
            .expect("write");
        assert_eq!(writer.path(), session.join(TRANSCRIPT_FILE));
    }
    {
        let mut writer = TranscriptWriter::create(&session, redactor).expect("reopen appends");
        writer
            .record(&TranscriptEvent::Notice {
                text: "second".into(),
            })
            .expect("write");
    }
    let parsed = lines(&session.join(TRANSCRIPT_FILE));
    assert_eq!(parsed.len(), 2);
    let raw = std::fs::read_to_string(session.join(TRANSCRIPT_FILE)).unwrap();
    assert!(!raw.contains(DUMMY_KEY));
    assert!(
        !raw.contains("abcdef123456"),
        "values under secret-looking keys are removed"
    );
    let validator = validator();
    assert!(parsed.iter().all(|l| validator.is_valid(l)));
}
