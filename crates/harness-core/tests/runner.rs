//! Fixture and process runners: replay, fail-closed parsing, exit handling, argv and secrets.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use harness_core::config::{RunnerKind, RunnerSection};
use harness_core::runner::{
    BenchmarkRunner, FixtureRunner, RunnerError, StageOutcome, StageRequest, build_argv,
    build_runner,
};
use harness_core::{
    CostLedger, CostStatus, EventBody, EventError, EventSource, FailureCategory, Metric, Phase,
    RateCard, RunEvent, Strategy, VerifierOutcome, read_events,
};
use tempfile::TempDir;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/runner")
}

fn request(strategy: Strategy, out: &Path) -> StageRequest {
    StageRequest {
        run_id: "run-42".into(),
        strategy,
        tool_kb_path: PathBuf::from("/data/kb/candidate.json"),
        server_version: "late".into(),
        task_ids: vec!["task-001".into(), "task-002".into()],
        provider: "mock".into(),
        model: "mock-model".into(),
        temperature: Some(0.0),
        top_p: None,
        max_output_tokens: Some(2048),
        seed: 7,
        judge_version: "judge-v1".into(),
        output_jsonl: out.join(format!("{strategy}.jsonl")),
        remaining_budget_usd: 12.5,
        mock: true,
    }
}

fn replay(strategy: Strategy) -> (Vec<RunEvent>, StageOutcome, TempDir) {
    let tmp = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(fixture_dir());
    let mut events = Vec::new();
    let outcome = runner
        .run_stage(&request(strategy, tmp.path()), &mut |e| events.push(e))
        .unwrap();
    (events, outcome, tmp)
}

fn verdict(events: &[RunEvent]) -> VerifierOutcome {
    events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::VerifierVerdict(v) => Some(v.outcome),
            _ => None,
        })
        .expect("a verdict event")
}

fn fixture_lines(strategy: Strategy) -> Vec<String> {
    fs::read_to_string(fixture_dir().join(format!("{strategy}.jsonl")))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Fixture directory in a temp dir with `no_update.jsonl` replaced by `lines`.
fn custom_fixture(lines: &[String]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(dir.path().join("no_update.jsonl"), text).unwrap();
    dir
}

fn replay_custom(dir: &TempDir) -> (Vec<RunEvent>, Result<StageOutcome, RunnerError>, TempDir) {
    let out = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(dir.path());
    let mut events = Vec::new();
    let result = runner.run_stage(&request(Strategy::NoUpdate, out.path()), &mut |e| {
        events.push(e)
    });
    (events, result, out)
}

// ----------------------------------------------------------------------------------------
// FixtureRunner: valid logs
// ----------------------------------------------------------------------------------------

#[test]
fn fixture_logs_replay_valid_events_with_rewritten_run_id() {
    for strategy in Strategy::ALL {
        let (events, outcome, _tmp) = replay(strategy);
        assert_eq!(outcome.events, events.len());
        assert_eq!(outcome.events, fixture_lines(strategy).len());
        assert!(outcome.exit.is_success());
        assert!(!outcome.budget_exhausted);
        assert!(!outcome.raw_redacted);
        assert!(
            events
                .iter()
                .all(|e| e.run_id == "run-42" && e.source == EventSource::Python)
        );
        assert!(events.iter().all(|e| e.strategy == Some(strategy)));
        // Sequence numbers stay monotonic as written by the (fixture) runner.
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(matches!(
            events.last().unwrap().body,
            EventBody::RunComplete(_)
        ));
    }
}

#[test]
fn raw_lines_are_preserved_verbatim() {
    let (_events, outcome, _tmp) = replay(Strategy::SelectivePatch);
    let raw = fs::read_to_string(&outcome.raw_path).unwrap();
    let original = fs::read_to_string(fixture_dir().join("selective_patch.jsonl")).unwrap();
    assert_eq!(raw, original);
    // The raw log is itself a valid event log.
    assert_eq!(
        read_events(&outcome.raw_path).unwrap().len(),
        outcome.events
    );
}

#[test]
fn fixtures_cover_the_three_verifier_outcomes() {
    let outcomes: Vec<_> = Strategy::ALL
        .iter()
        .map(|s| verdict(&replay(*s).0))
        .collect();
    assert_eq!(
        outcomes,
        [
            VerifierOutcome::ConfirmedFailure,
            VerifierOutcome::Inconclusive,
            VerifierOutcome::ConfirmedSuccess
        ]
    );
}

#[test]
fn fixtures_carry_all_five_metric_ids() {
    let mut seen = BTreeSet::new();
    for strategy in Strategy::ALL {
        let metrics: BTreeSet<Metric> = replay(strategy)
            .0
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::MetricScore(m) => Some(m.metric),
                _ => None,
            })
            .collect();
        assert_eq!(metrics.len(), 5, "{strategy}: {metrics:?}");
        seen.extend(metrics);
    }
    assert_eq!(
        seen,
        BTreeSet::from([
            Metric::Tf,
            Metric::Pe,
            Metric::Ecs,
            Metric::Recovery,
            Metric::Retention
        ])
    );
}

#[test]
fn fixtures_carry_both_failure_categories() {
    let categories: BTreeSet<FailureCategory> = Strategy::ALL
        .iter()
        .flat_map(|s| replay(*s).0)
        .filter_map(|e| match e.body {
            EventBody::FailureCategory(f) => Some(f.category),
            _ => None,
        })
        .collect();
    assert_eq!(
        categories,
        BTreeSet::from([FailureCategory::UnderUpdate, FailureCategory::OverUpdate])
    );
}

#[test]
fn python_cost_events_aggregate_through_the_ledger() {
    let mut ledger = CostLedger::new(100.0, RateCard::empty("rc", "2026-09-01"));
    let mut mock_calls = 0;
    for strategy in Strategy::ALL {
        for event in replay(strategy).0 {
            if let EventBody::ModelCall(call) = &event.body {
                assert_eq!(call.cost_status, CostStatus::Mock);
                ledger.ingest_external(call).unwrap();
                mock_calls += 1;
            }
        }
    }
    assert_eq!(mock_calls, 6);
    let totals = ledger.totals_by_phase();
    assert_eq!(totals[&Phase::Agent].calls, 3);
    assert_eq!(totals[&Phase::Verifier].calls, 3);
    assert_eq!(totals[&Phase::Agent].mock_calls, 3);
    assert!(totals[&Phase::Agent].input_tokens > 0);
    assert_eq!(ledger.spent_usd(), 0.0, "mock usage is never USD");
    assert_eq!(ledger.remaining_usd(), 100.0);
}

#[test]
fn priced_and_unknown_python_costs_are_aggregated_not_zeroed() {
    let lines = vec![
        r#"{"schema_version":"run-event/1","run_id":"x","seq":0,"ts":"2026-09-01T10:00:00Z","source":"python","strategy":null,"event_type":"model_call","provider":"openai","model":"gpt","phase":"verifier","request_id":"r1","input_tokens":1000,"output_tokens":100,"latency_ms":5,"rate_card_id":"rc","rate_card_date":"2026-09-01","estimated_usd":0.25,"cost_status":"priced"}"#.to_owned(),
        r#"{"schema_version":"run-event/1","run_id":"x","seq":1,"ts":"2026-09-01T10:00:01Z","source":"python","strategy":null,"event_type":"model_call","provider":"openai","model":"gpt","phase":"probe_generation","request_id":"r2","input_tokens":null,"output_tokens":null,"latency_ms":5,"rate_card_id":null,"rate_card_date":null,"estimated_usd":null,"cost_status":"unknown"}"#.to_owned(),
    ];
    let dir = custom_fixture(&lines);
    let (events, result, _out) = replay_custom(&dir);
    result.unwrap();
    let mut ledger = CostLedger::new(10.0, RateCard::empty("rc", "2026-09-01"));
    for event in &events {
        if let EventBody::ModelCall(call) = &event.body {
            ledger.ingest_external(call).unwrap();
        }
    }
    assert!((ledger.spent_usd() - 0.25).abs() < 1e-12);
    let totals = ledger.totals_by_phase();
    assert_eq!(totals[&Phase::ProbeGeneration].unknown_calls, 1);
    assert!((totals[&Phase::Verifier].usd - 0.25).abs() < 1e-12);
    // strategy null in the log is filled from the request.
    assert!(
        events
            .iter()
            .all(|e| e.strategy == Some(Strategy::NoUpdate))
    );
}

// ----------------------------------------------------------------------------------------
// FixtureRunner: invalid logs fail closed and name the line
// ----------------------------------------------------------------------------------------

#[test]
fn malformed_line_fails_closed_naming_the_line() {
    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines[2] = "{ this is not json".to_owned();
    let dir = custom_fixture(&lines);
    let (events, result, out) = replay_custom(&dir);
    let err = result.unwrap_err();
    assert!(err.to_string().contains("line 3"), "{err}");
    match err {
        RunnerError::InvalidEvent {
            source,
            events_before,
            raw_path,
        } => {
            assert_eq!(source.line(), Some(3));
            assert_eq!(events_before, 2);
            assert_eq!(events.len(), 2, "valid prefix was delivered");
            let raw = fs::read_to_string(raw_path).unwrap();
            assert_eq!(raw.lines().count(), 3, "raw log keeps what was seen");
        }
        other => panic!("unexpected {other:?}"),
    }
    drop(out);
}

#[test]
fn unsupported_schema_version_fails_closed() {
    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines[1] = lines[1].replace("run-event/1", "run-event/2");
    let dir = custom_fixture(&lines);
    let (events, result, _out) = replay_custom(&dir);
    assert_eq!(events.len(), 1);
    match result.unwrap_err() {
        RunnerError::InvalidEvent { source, .. } => {
            assert!(matches!(
                source,
                EventError::UnsupportedSchemaVersion { line: 2, .. }
            ));
            assert!(source.to_string().contains("line 2"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn schema_violation_and_blank_line_fail_closed() {
    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines[4] = lines[4].replace(
        "\"event_type\":\"model_call\"",
        "\"event_type\":\"nonsense\"",
    );
    let dir = custom_fixture(&lines);
    let (_events, result, _out) = replay_custom(&dir);
    let err = result.unwrap_err();
    assert!(err.to_string().contains("line 5"), "{err}");

    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines.insert(1, String::new());
    let dir = custom_fixture(&lines);
    let (_events, result, _out) = replay_custom(&dir);
    let err = result.unwrap_err();
    assert!(err.to_string().contains("line 2"), "{err}");
    assert!(err.to_string().contains("blank"), "{err}");
}

#[test]
fn rust_source_events_from_the_runner_are_rejected() {
    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines[0] = lines[0].replace("\"source\":\"python\"", "\"source\":\"rust\"");
    let dir = custom_fixture(&lines);
    let (_events, result, _out) = replay_custom(&dir);
    match result.unwrap_err() {
        RunnerError::Contract { line, .. } => assert_eq!(line, 1),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn missing_fixture_is_reported() {
    let out = TempDir::new().unwrap();
    let empty = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(empty.path());
    let err = runner
        .run_stage(&request(Strategy::FullRewrite, out.path()), &mut |_| {})
        .unwrap_err();
    assert!(matches!(err, RunnerError::MissingFixture { .. }));
}

// ----------------------------------------------------------------------------------------
// FixtureRunner: exits and partial logs
// ----------------------------------------------------------------------------------------

#[test]
fn simulated_nonzero_exit_keeps_partial_events() {
    let out = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(fixture_dir())
        .with_max_events(4)
        .with_simulated_exit(3);
    let mut events = Vec::new();
    let err = runner
        .run_stage(&request(Strategy::NoUpdate, out.path()), &mut |e| {
            events.push(e)
        })
        .unwrap_err();
    match err {
        RunnerError::ChildExit {
            exit,
            events: n,
            raw_path,
            ..
        } => {
            assert_eq!(exit.code, Some(3));
            assert_eq!(n, 4);
            assert_eq!(events.len(), 4);
            assert_eq!(fs::read_to_string(raw_path).unwrap().lines().count(), 4);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn simulated_exit_two_is_budget_exhausted_with_valid_partial_log() {
    let out = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(fixture_dir())
        .with_max_events(6)
        .with_simulated_exit(2);
    let outcome = runner
        .run_stage(&request(Strategy::FullRewrite, out.path()), &mut |_| {})
        .unwrap();
    assert!(outcome.budget_exhausted);
    assert_eq!(outcome.exit.code, Some(2));
    assert_eq!(outcome.events, 6);
    assert_eq!(read_events(&outcome.raw_path).unwrap().len(), 6);
}

#[test]
fn run_complete_budget_exhausted_sets_the_flag() {
    let mut lines = fixture_lines(Strategy::NoUpdate);
    let last = lines.len() - 1;
    lines[last] = lines[last].replace("\"status\":\"ok\"", "\"status\":\"budget_exhausted\"");
    let dir = custom_fixture(&lines);
    let (_events, result, _out) = replay_custom(&dir);
    assert!(result.unwrap().budget_exhausted);
}

#[test]
fn fixture_runner_redacts_secrets_from_events_and_raw_log() {
    let secret = "zz-fixture-secret-1234";
    let mut lines = fixture_lines(Strategy::NoUpdate);
    lines[0] = lines[0].replace("Login page returns 500", secret);
    let dir = custom_fixture(&lines);
    let out = TempDir::new().unwrap();
    let mut runner = FixtureRunner::new(dir.path())
        .with_redactor(harness_core::Redactor::new(vec![secret.to_owned()]));
    let mut events = Vec::new();
    let outcome = runner
        .run_stage(&request(Strategy::NoUpdate, out.path()), &mut |e| {
            events.push(e)
        })
        .unwrap();
    assert!(outcome.raw_redacted);
    assert!(
        !fs::read_to_string(&outcome.raw_path)
            .unwrap()
            .contains(secret)
    );
    assert!(!serde_json::to_string(&events).unwrap().contains(secret));
}

// ----------------------------------------------------------------------------------------
// build_argv
// ----------------------------------------------------------------------------------------

fn argv_strings(req: &StageRequest) -> Vec<String> {
    build_argv(req)
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn argv_follows_the_proposed_contract_exactly() {
    let out = TempDir::new().unwrap();
    let req = request(Strategy::SelectivePatch, out.path());
    let argv = argv_strings(&req);
    let expected: Vec<String> = [
        "run",
        "--run-id",
        "run-42",
        "--tool-kb",
        "/data/kb/candidate.json",
        "--server-version",
        "late",
        "--task-id",
        "task-001",
        "--task-id",
        "task-002",
        "--provider",
        "mock",
        "--model",
        "mock-model",
        "--temperature",
        "0",
        "--max-output-tokens",
        "2048",
        "--seed",
        "7",
        "--judge-version",
        "judge-v1",
        "--output-jsonl",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(&argv[..expected.len()], expected.as_slice());
    assert_eq!(
        argv[expected.len()],
        req.output_jsonl.to_string_lossy().into_owned()
    );
    assert_eq!(
        &argv[expected.len() + 1..],
        ["--remaining-budget-usd", "12.5", "--mock"]
    );
}

#[test]
fn argv_omits_unset_options_and_mock_flag() {
    let out = TempDir::new().unwrap();
    let mut req = request(Strategy::NoUpdate, out.path());
    req.temperature = None;
    req.max_output_tokens = None;
    req.top_p = Some(0.9);
    req.mock = false;
    let argv = argv_strings(&req);
    assert!(!argv.contains(&"--temperature".to_owned()));
    assert!(!argv.contains(&"--max-output-tokens".to_owned()));
    assert!(!argv.contains(&"--mock".to_owned()));
    let pos = argv.iter().position(|a| a == "--top-p").unwrap();
    assert_eq!(argv[pos + 1], "0.9");
}

// ----------------------------------------------------------------------------------------
// build_runner
// ----------------------------------------------------------------------------------------

#[test]
fn build_runner_selects_by_config_and_validates() {
    let section = RunnerSection {
        kind: RunnerKind::Fixture,
        fixture_dir: Some(fixture_dir()),
        executable: None,
        fixed_args: vec![],
    };
    let out = TempDir::new().unwrap();
    let mut runner = build_runner(&section, &[]).unwrap();
    let outcome = runner
        .run_stage(&request(Strategy::SelectivePatch, out.path()), &mut |_| {})
        .unwrap();
    assert!(outcome.events > 0);

    let missing_dir = RunnerSection {
        fixture_dir: None,
        ..section.clone()
    };
    assert!(matches!(
        build_runner(&missing_dir, &[]),
        Err(RunnerError::InvalidRequest(_))
    ));
    let process_without_exe = RunnerSection {
        kind: RunnerKind::Process,
        fixture_dir: None,
        executable: None,
        fixed_args: vec![],
    };
    assert!(matches!(
        build_runner(&process_without_exe, &[]),
        Err(RunnerError::InvalidRequest(_))
    ));
    let process = RunnerSection {
        kind: RunnerKind::Process,
        fixture_dir: None,
        executable: Some("python".into()),
        fixed_args: vec!["-m".into(), "runner".into()],
    };
    assert!(build_runner(&process, &["MCP_DATA_DIR".to_owned()]).is_ok());
}

// ----------------------------------------------------------------------------------------
// ProcessRunner (Unix: uses temp #!/bin/sh scripts)
// ----------------------------------------------------------------------------------------

#[cfg(unix)]
mod process {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use harness_core::runner::ProcessRunner;

    use super::*;

    const SECRET: &str = "sk-test-0123456789abcdef-SECRET";

    /// Fake runner. Fixed args: `<record-file> <log-to-copy> <exit-code> [<lines-to-copy>]`.
    /// It records its argv (one per line) and whether selected env vars are present, then
    /// writes the log to the `--output-jsonl` path and exits with the given code.
    const SCRIPT: &str = r#"#!/bin/sh
rec="$1"; src="$2"; code="$3"; lines="$4"
shift 4
: > "$rec"
for a in "$@"; do printf '%s\n' "$a" >> "$rec"; done
[ -n "${OPENAI_API_KEY:-}" ] && echo "ENV:OPENAI_API_KEY=present" >> "$rec"
[ -n "${CARGO_MANIFEST_DIR:-}" ] && echo "ENV:CARGO_MANIFEST_DIR=present" >> "$rec"
[ -n "${MCP_DATA_DIR:-}" ] && echo "ENV:MCP_DATA_DIR=$MCP_DATA_DIR" >> "$rec"
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--output-jsonl" ]; then out="$a"; fi
  prev="$a"
done
if [ -n "$lines" ] && [ "$lines" != "all" ]; then
  head -n "$lines" "$src" > "$out"
else
  cp "$src" "$out"
fi
exit "$code"
"#;

    struct Harness {
        _tmp: TempDir,
        script: PathBuf,
        record: PathBuf,
        log: PathBuf,
        out_dir: PathBuf,
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Child log = the fixture for `strategy` with the run id rewritten to "run-42".
    fn harness(strategy: Strategy) -> Harness {
        let tmp = TempDir::new().unwrap();
        let script = write_script(tmp.path(), "runner.sh", SCRIPT);
        let record = tmp.path().join("record.txt");
        let log = tmp.path().join("child-log.jsonl");
        let text: String = fixture_lines(strategy)
            .iter()
            .map(|l| format!("{}\n", l.replace("smoke-fixture", "run-42")))
            .collect();
        fs::write(&log, text).unwrap();
        let out_dir = tmp.path().join("out");
        Harness {
            _tmp: tmp,
            script,
            record,
            log,
            out_dir,
        }
    }

    fn runner_for(h: &Harness, code: i32, lines: &str) -> ProcessRunner {
        ProcessRunner::new(
            h.script.to_string_lossy().into_owned(),
            vec![
                h.record.to_string_lossy().into_owned(),
                h.log.to_string_lossy().into_owned(),
                code.to_string(),
                lines.to_owned(),
            ],
        )
    }

    fn secret_env(name: &str) -> Option<String> {
        (name == "OPENAI_API_KEY").then(|| SECRET.to_owned())
    }

    #[test]
    fn process_runner_parses_events_and_passes_no_secret_in_argv() {
        let h = harness(Strategy::SelectivePatch);
        let mut runner = runner_for(&h, 0, "all")
            .with_passthrough_env(["OPENAI_API_KEY", "MCP_DATA_DIR"])
            .with_env_lookup(|name| match name {
                "OPENAI_API_KEY" => Some(SECRET.to_owned()),
                "MCP_DATA_DIR" => Some("/data/catalog".to_owned()),
                _ => None,
            });
        let req = request(Strategy::SelectivePatch, &h.out_dir);
        let mut events = Vec::new();
        let outcome = runner.run_stage(&req, &mut |e| events.push(e)).unwrap();

        assert!(outcome.exit.is_success());
        assert_eq!(outcome.events, 13);
        assert_eq!(events.len(), 13);
        assert!(!outcome.budget_exhausted);
        assert_eq!(verdict(&events), VerifierOutcome::ConfirmedSuccess);
        assert!(events.iter().all(|e| e.run_id == "run-42"));

        // The child saw exactly the contract argv, one argument per line, no shell.
        let record = fs::read_to_string(&h.record).unwrap();
        let recorded: Vec<&str> = record.lines().filter(|l| !l.starts_with("ENV:")).collect();
        let expected: Vec<String> = argv_strings(&req);
        assert_eq!(
            recorded,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(recorded[0], "run");
        assert!(
            !recorded
                .iter()
                .any(|a| matches!(*a, "sh" | "-c" | "bash" | "/bin/sh"))
        );

        // No secret in argv, the record, the raw log or the delivered events.
        assert!(!record.contains(SECRET));
        assert!(
            !fs::read_to_string(&outcome.raw_path)
                .unwrap()
                .contains(SECRET)
        );
        assert!(!serde_json::to_string(&events).unwrap().contains(SECRET));

        // The key reached the child only through the environment (presence recorded, not value).
        assert!(record.contains("ENV:OPENAI_API_KEY=present"));
        assert!(record.contains("ENV:MCP_DATA_DIR=/data/catalog"));
    }

    #[test]
    fn child_environment_is_cleared_except_named_variables() {
        let h = harness(Strategy::NoUpdate);
        // CARGO_MANIFEST_DIR is set by cargo for the test process but was not named.
        let mut runner = runner_for(&h, 0, "all").with_env_lookup(secret_env);
        runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |_| {})
            .unwrap();
        let record = fs::read_to_string(&h.record).unwrap();
        assert!(!record.contains("ENV:CARGO_MANIFEST_DIR"), "{record}");
        assert!(
            !record.contains("ENV:OPENAI_API_KEY"),
            "an unnamed variable must not reach the child: {record}"
        );
    }

    #[test]
    fn credential_shaped_argument_is_refused_before_spawn() {
        let h = harness(Strategy::NoUpdate);
        let mut runner = ProcessRunner::new(
            h.script.to_string_lossy().into_owned(),
            vec![
                h.record.to_string_lossy().into_owned(),
                format!("--api-key={SECRET}"),
            ],
        )
        .with_passthrough_env(["OPENAI_API_KEY"])
        .with_env_lookup(secret_env);
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, RunnerError::InvalidRequest(_)), "{err}");
        assert!(!err.to_string().contains(SECRET));
        assert!(!h.record.exists(), "the child must not have been started");
    }

    #[test]
    fn leaked_secret_in_child_output_is_scrubbed_from_events_and_raw_log() {
        let h = harness(Strategy::NoUpdate);
        let leaked =
            fs::read_to_string(&h.log)
                .unwrap()
                .replacen("Login page returns 500", SECRET, 1);
        fs::write(&h.log, leaked).unwrap();
        let mut runner = runner_for(&h, 0, "all")
            .with_passthrough_env(["OPENAI_API_KEY"])
            .with_env_lookup(secret_env);
        let mut events = Vec::new();
        let outcome = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |e| {
                events.push(e)
            })
            .unwrap();
        assert!(outcome.raw_redacted);
        assert!(
            !fs::read_to_string(&outcome.raw_path)
                .unwrap()
                .contains(SECRET)
        );
        assert!(!serde_json::to_string(&events).unwrap().contains(SECRET));
        assert_eq!(
            read_events(&outcome.raw_path).unwrap().len(),
            outcome.events
        );
    }

    #[test]
    fn nonzero_exit_returns_child_exit_with_partial_events_preserved() {
        let h = harness(Strategy::NoUpdate);
        let mut runner = runner_for(&h, 3, "5");
        let mut events = Vec::new();
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |e| {
                events.push(e)
            })
            .unwrap_err();
        match err {
            RunnerError::ChildExit {
                exit,
                events: n,
                raw_path,
                ..
            } => {
                assert_eq!(exit.code, Some(3));
                assert_eq!(n, 5);
                assert_eq!(events.len(), 5, "partial events were delivered");
                assert_eq!(
                    read_events(&raw_path).unwrap().len(),
                    5,
                    "partial log preserved"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn exit_code_two_means_budget_exhausted_with_valid_partial_log() {
        let h = harness(Strategy::FullRewrite);
        let mut runner = runner_for(&h, 2, "7");
        let mut events = Vec::new();
        let outcome = runner
            .run_stage(&request(Strategy::FullRewrite, &h.out_dir), &mut |e| {
                events.push(e)
            })
            .unwrap();
        assert!(outcome.budget_exhausted);
        assert_eq!(outcome.exit.code, Some(2));
        assert_eq!(outcome.events, 7);
        assert_eq!(read_events(&outcome.raw_path).unwrap().len(), 7);
    }

    #[test]
    fn malformed_line_from_child_fails_closed_naming_the_line() {
        let h = harness(Strategy::NoUpdate);
        let mut lines: Vec<String> = fs::read_to_string(&h.log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines[3] = "garbage".into();
        fs::write(&h.log, lines.join("\n") + "\n").unwrap();
        let mut runner = runner_for(&h, 0, "all");
        let mut events = Vec::new();
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |e| {
                events.push(e)
            })
            .unwrap_err();
        assert!(err.to_string().contains("line 4"), "{err}");
        assert!(matches!(err, RunnerError::InvalidEvent { .. }));
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn unsupported_schema_version_from_child_fails_closed() {
        let h = harness(Strategy::NoUpdate);
        let text = fs::read_to_string(&h.log)
            .unwrap()
            .replacen("run-event/1", "run-event/9", 1);
        fs::write(&h.log, text).unwrap();
        let mut runner = runner_for(&h, 0, "all");
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |_| {})
            .unwrap_err();
        match err {
            RunnerError::InvalidEvent { source, .. } => {
                assert!(matches!(
                    source,
                    EventError::UnsupportedSchemaVersion { line: 1, .. }
                ));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wrong_run_id_from_child_is_a_contract_violation() {
        let h = harness(Strategy::NoUpdate);
        let text = fs::read_to_string(&h.log)
            .unwrap()
            .replace("run-42", "other-run");
        fs::write(&h.log, text).unwrap();
        let mut runner = runner_for(&h, 0, "all");
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(err, RunnerError::Contract { line: 1, .. }),
            "{err}"
        );
    }

    #[test]
    fn unterminated_final_line_after_a_failed_exit_does_not_mask_the_exit() {
        let h = harness(Strategy::NoUpdate);
        let mut text = fs::read_to_string(&h.log).unwrap();
        text.truncate(text.len() - 20); // cut the last line mid-way, no trailing newline
        fs::write(&h.log, text).unwrap();
        let mut runner = runner_for(&h, 1, "all");
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, &h.out_dir), &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, RunnerError::ChildExit { .. }), "{err}");
    }

    #[test]
    fn lines_are_consumed_incrementally_while_the_child_runs() {
        let tmp = TempDir::new().unwrap();
        let log: Vec<String> = fixture_lines(Strategy::NoUpdate)
            .iter()
            .map(|l| l.replace("smoke-fixture", "run-42"))
            .collect();
        let (part_a, part_b) = log[1].split_at(30);
        let files = [
            (tmp.path().join("p1"), format!("{}\n", log[0])),
            (tmp.path().join("p2"), part_a.to_owned()),
            (
                tmp.path().join("p3"),
                format!("{part_b}\n{}\n", log[2..].join("\n")),
            ),
        ];
        for (path, content) in &files {
            fs::write(path, content).unwrap();
        }
        // Writes line 1, half of line 2 (no newline), pauses, then finishes the rest.
        let body = "#!/bin/sh\np1=\"$1\"; p2=\"$2\"; p3=\"$3\"\nshift 3\nout=\"\"\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--output-jsonl\" ]; then out=\"$a\"; fi\n  prev=\"$a\"\ndone\ncat \"$p1\" > \"$out\"\ncat \"$p2\" >> \"$out\"\nsleep 0.3\ncat \"$p3\" >> \"$out\"\nexit 0\n";
        let script = write_script(tmp.path(), "slow.sh", body);
        let mut runner = ProcessRunner::new(
            script.to_string_lossy().into_owned(),
            files
                .iter()
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect(),
        );
        let mut events = Vec::new();
        let outcome = runner
            .run_stage(
                &request(Strategy::NoUpdate, &tmp.path().join("o")),
                &mut |e| events.push(e),
            )
            .unwrap();
        assert_eq!(outcome.events, log.len());
        assert_eq!(events.len(), log.len());
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[test]
    fn cancel_flag_stops_the_child_and_keeps_partial_events() {
        let tmp = TempDir::new().unwrap();
        let first = tmp.path().join("first");
        fs::write(
            &first,
            format!(
                "{}\n",
                fixture_lines(Strategy::NoUpdate)[0].replace("smoke-fixture", "run-42")
            ),
        )
        .unwrap();
        let body = "#!/bin/sh\nsrc=\"$1\"\nshift 1\nout=\"\"\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--output-jsonl\" ]; then out=\"$a\"; fi\n  prev=\"$a\"\ndone\ncat \"$src\" > \"$out\"\nexec sleep 30\n";
        let script = write_script(tmp.path(), "hang.sh", body);
        let flag = Arc::new(AtomicBool::new(false));
        let mut runner = ProcessRunner::new(
            script.to_string_lossy().into_owned(),
            vec![first.to_string_lossy().into_owned()],
        )
        .with_cancel_flag(flag.clone());
        let mut events = Vec::new();
        let setter = flag.clone();
        let err = runner
            .run_stage(
                &request(Strategy::NoUpdate, &tmp.path().join("o")),
                &mut |e| {
                    events.push(e);
                    setter.store(true, Ordering::Relaxed);
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, RunnerError::Cancelled { events: 1, .. }),
            "{err}"
        );
    }

    #[test]
    fn missing_executable_is_a_spawn_error() {
        let tmp = TempDir::new().unwrap();
        let mut runner = ProcessRunner::new("/nonexistent/runner-binary", vec![]);
        let err = runner
            .run_stage(&request(Strategy::NoUpdate, tmp.path()), &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, RunnerError::Spawn { .. }), "{err}");
    }
}
