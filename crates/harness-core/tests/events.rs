//! EventEmitter, sinks and redaction at the emit choke point.

use std::fs;
use std::sync::{Arc, Mutex};

use harness_core::{
    CostStatus, ErrorEvent, EventBody, EventEmitter, EventError, EventSink, EventSource,
    FixedClock, FnSink, JsonlFileSink, McpActionEvent, ModelCallEvent, Phase, Redactor,
    RunCompleteEvent, RunEvent, RunStatus, Strategy, TeeSink, VecSink, read_events,
};
use serde_json::json;

const SECRET: &str = "hunter2-very-secret";

fn clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new("2026-09-01T10:00:00Z"))
}

fn error_body(message: &str) -> EventBody {
    EventBody::Error(ErrorEvent {
        message: message.to_owned(),
        fatal: false,
    })
}

fn emitter_with(sink: Box<dyn EventSink + Send>, redactor: Redactor) -> EventEmitter {
    EventEmitter::new("run-1", EventSource::Rust, clock(), redactor, sink)
}

fn model_call() -> EventBody {
    EventBody::ModelCall(ModelCallEvent {
        provider: "mock".into(),
        model: "mock-model".into(),
        phase: Phase::Adaptation,
        request_id: "mock-1".into(),
        input_tokens: Some(10),
        output_tokens: Some(5),
        latency_ms: 1,
        rate_card_id: None,
        rate_card_date: None,
        estimated_usd: None,
        cost_status: CostStatus::Mock,
    })
}

#[test]
fn emitter_stamps_envelope_and_sequences() {
    let sink = VecSink::new();
    let mut emitter = emitter_with(Box::new(sink.clone()), Redactor::default());
    let first = emitter
        .emit(Some(Strategy::NoUpdate), error_body("a"))
        .unwrap();
    let second = emitter.emit(None, model_call()).unwrap();

    assert_eq!(first.seq, 0);
    assert_eq!(second.seq, 1);
    assert_eq!(first.schema_version, "run-event/1");
    assert_eq!(first.run_id, "run-1");
    assert_eq!(first.ts, "2026-09-01T10:00:00Z");
    assert_eq!(first.source, EventSource::Rust);
    assert_eq!(first.strategy, Some(Strategy::NoUpdate));
    assert_eq!(second.strategy, None);
    assert_eq!(sink.events(), vec![first, second]);
    assert_eq!(emitter.next_seq(), 2);
}

#[test]
fn emitter_redacts_before_the_sink_sees_the_event() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_in_sink = Arc::clone(&seen);
    let sink = FnSink::new(move |event: &RunEvent| {
        seen_in_sink.lock().unwrap().push(event.to_json_line()?);
        Ok(())
    });
    let mut emitter = emitter_with(Box::new(sink), Redactor::new(vec![SECRET.to_owned()]));

    let returned = emitter
        .emit(
            None,
            EventBody::McpAction(McpActionEvent {
                task_id: "t".into(),
                step: 1,
                tool_id: "x".into(),
                arguments: json!({
                    "note": format!("token is {SECRET}"),
                    "nested": {"key": "sk-abcdefghijklmnopqrstuvwxyz"},
                    "password": "whatever"
                })
                .as_object()
                .unwrap()
                .clone(),
                result_summary: format!("echoed {SECRET}"),
                is_error: false,
            }),
        )
        .unwrap();

    let lines = seen.lock().unwrap();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].contains(SECRET), "{}", lines[0]);
    assert!(!lines[0].contains("sk-abcdef"), "{}", lines[0]);
    assert!(!lines[0].contains("whatever"), "{}", lines[0]);
    assert!(lines[0].contains("[REDACTED]"));
    // The value handed back to the caller is the redacted one too.
    assert!(!returned.to_json_line().unwrap().contains(SECRET));
}

#[test]
fn emitter_rejects_events_that_fail_the_schema_and_does_not_advance_seq() {
    let sink = VecSink::new();
    let mut emitter = emitter_with(Box::new(sink.clone()), Redactor::default());
    // Empty run_id is invalid; an empty task_id violates the schema's minLength.
    let bad = EventBody::VerifierProbe(harness_core::VerifierProbeEvent {
        task_id: String::new(),
        probe_id: "p".into(),
        description: "d".into(),
        observation: "o".into(),
    });
    let err = emitter.emit(None, bad).expect_err("schema violation");
    assert!(matches!(err, EventError::Invalid { .. }), "{err}");
    assert!(sink.is_empty());
    assert_eq!(emitter.next_seq(), 0);
}

#[test]
fn forward_redacts_and_validates_without_restamping() {
    let sink = VecSink::new();
    let mut emitter = emitter_with(
        Box::new(sink.clone()),
        Redactor::new(vec![SECRET.to_owned()]),
    );
    let event = RunEvent {
        schema_version: "run-event/1".into(),
        run_id: "other-run".into(),
        seq: 41,
        ts: "2026-01-02T03:04:05Z".into(),
        source: EventSource::Python,
        strategy: Some(Strategy::FullRewrite),
        body: error_body(&format!("python saw {SECRET}")),
    };
    let out = emitter.forward(&event).unwrap();
    assert_eq!(out.seq, 41);
    assert_eq!(out.run_id, "other-run");
    assert_eq!(out.source, EventSource::Python);
    assert!(matches!(&out.body, EventBody::Error(e) if e.message == "python saw [REDACTED]"));
    assert_eq!(
        emitter.next_seq(),
        0,
        "forward does not consume rust seq numbers"
    );
}

#[test]
fn jsonl_sink_writes_one_valid_deterministic_line_per_event() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str| {
        let path = dir.path().join(name);
        let sink = JsonlFileSink::open(&path).unwrap();
        let mut emitter = emitter_with(Box::new(sink), Redactor::default());
        emitter
            .emit(Some(Strategy::SelectivePatch), model_call())
            .unwrap();
        emitter
            .emit(Some(Strategy::SelectivePatch), error_body("boom"))
            .unwrap();
        emitter
            .emit(
                None,
                EventBody::RunComplete(RunCompleteEvent {
                    status: RunStatus::Ok,
                    detail: None,
                }),
            )
            .unwrap();
        path
    };
    let a = write("a.jsonl");
    let b = write("nested/b.jsonl");

    let text_a = fs::read_to_string(&a).unwrap();
    assert_eq!(
        text_a,
        fs::read_to_string(&b).unwrap(),
        "byte-identical with FixedClock"
    );
    assert_eq!(text_a.lines().count(), 3);
    assert!(text_a.ends_with('\n'));

    // Keys are sorted, so the first field is `cost_status`-style alphabetical, not the envelope.
    let first = text_a.lines().next().unwrap();
    assert!(first.starts_with("{\"cost_status\":\"mock\""), "{first}");
    assert!(first.contains("\"ts\":\"2026-09-01T10:00:00Z\""));

    let events = read_events(&a).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn jsonl_sink_appends_to_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    for _ in 0..2 {
        let sink = JsonlFileSink::open(&path).unwrap();
        let mut emitter = emitter_with(Box::new(sink), Redactor::default());
        emitter.emit(None, error_body("x")).unwrap();
    }
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
}

#[test]
fn tee_sink_fans_out_and_fn_sink_errors_propagate() {
    let a = VecSink::new();
    let b = VecSink::new();
    let mut tee = TeeSink::new(vec![Box::new(a.clone())]);
    tee.push(Box::new(b.clone()));
    let mut emitter = emitter_with(Box::new(tee), Redactor::default());
    emitter.emit(None, error_body("x")).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);

    let failing = FnSink::new(|_: &RunEvent| Err(EventError::Sink("disk full".into())));
    let mut emitter = emitter_with(Box::new(failing), Redactor::default());
    let err = emitter
        .emit(None, error_body("x"))
        .expect_err("sink failure");
    assert!(err.to_string().contains("disk full"));
    assert_eq!(
        emitter.next_seq(),
        0,
        "seq not consumed when the write failed"
    );
}
