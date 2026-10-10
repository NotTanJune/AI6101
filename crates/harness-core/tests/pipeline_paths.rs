//! Failure, refusal, cancellation and secret-handling paths of the pipeline. No network: the
//! providers are the in-process mock and the runners replay fixtures.

mod pipeline_common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use harness_core::canary::{ProbeObservation, ScriptedExecutor};
use harness_core::cost::{RateCard, RateEntry};
use harness_core::pipeline::{EnvLookup, ProbeExecutorFactory, StageStatus, run_matrix};
use harness_core::provider::MockProvider;
use harness_core::runner::FixtureRunner;
use harness_core::{
    AdaptationStatus, CanaryStatus, ConfigProviderKind, CostStatus, EventBody, FnSink,
    ProviderKind, RunEvent, RunStatus, Strategy, VecSink,
};
use pipeline_common::{
    all_files, copy_dir, opts, read_run_events, read_text, repo_root, smoke_config,
};

fn outcome_of(events: &[RunEvent], strategy: Strategy) -> harness_core::AdaptationOutcomeEvent {
    events
        .iter()
        .filter(|e| e.strategy == Some(strategy))
        .find_map(|e| match &e.body {
            EventBody::AdaptationOutcome(o) => Some(o.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no adaptation_outcome for {strategy}"))
}

fn count_type(events: &[RunEvent], strategy: Strategy, event_type: &str) -> usize {
    events
        .iter()
        .filter(|e| e.strategy == Some(strategy) && e.body.event_type() == event_type)
        .count()
}

fn final_status(events: &[RunEvent]) -> (RunStatus, Option<String>) {
    match &events.last().expect("events").body {
        EventBody::RunComplete(done) => (done.status, done.detail.clone()),
        other => panic!("last event is {other:?}"),
    }
}

fn failing_canary(isolated: bool) -> ProbeExecutorFactory {
    Arc::new(move |_target, _strategy| {
        Box::new(
            ScriptedExecutor::new(isolated)
                .with_default(ProbeObservation::fail("target rejected the call")),
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Blocked canary
// ---------------------------------------------------------------------------------------------

#[test]
fn failed_canary_blocks_promotion_and_the_runner_gets_the_prior_kb() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_probe_executor(failing_canary(true)),
    )
    .unwrap();
    let events = read_run_events(dir.path());

    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let o = outcome_of(&events, strategy);
        assert_eq!(o.status, AdaptationStatus::Blocked, "{strategy}");
        assert!(o.reason.contains("canary failed"), "{}", o.reason);
        assert!(
            o.reason.contains("prior knowledge base kept"),
            "{}",
            o.reason
        );
        assert!(
            o.candidate_hash.is_some(),
            "the blocked candidate is still identified"
        );

        // The KB handed to the runner is the PRIOR KB, byte for byte, never the candidate.
        let bytes = std::fs::read(
            dir.path()
                .join(format!("kb/{}-rep1.json", strategy.as_str())),
        )
        .unwrap();
        assert_eq!(hex_sha256(&bytes), o.prior_hash);
        assert_ne!(Some(o.prior_hash.clone()), o.candidate_hash);

        let entry = summary.entry(strategy, 1).unwrap();
        assert_eq!(
            entry.adaptation.as_ref().unwrap().status,
            AdaptationStatus::Blocked
        );
        assert_eq!(entry.canary_status, Some(CanaryStatus::Fail));
        assert_eq!(
            entry.stage,
            StageStatus::Completed,
            "the runner still records the prior KB"
        );
        assert_eq!(entry.active_kb_hash.as_deref(), Some(o.prior_hash.as_str()));
        // A failed canary is never reported as a promotion.
        assert!(!events.iter().any(|e| matches!(&e.body,
                EventBody::AdaptationOutcome(o) if o.status == AdaptationStatus::Promoted)));
    }
    // No-update is unaffected.
    assert_eq!(
        outcome_of(&events, Strategy::NoUpdate).status,
        AdaptationStatus::Unchanged
    );

    let (status, detail) = final_status(&events);
    assert_eq!(status, RunStatus::Ok);
    assert!(detail.unwrap().contains("blocked or rejected"));
    assert!(summary.to_string().contains("adaptation: blocked"));
}

#[test]
fn inconclusive_canary_also_blocks() {
    let dir = tempfile::tempdir().unwrap();
    // A non-isolated executor never runs the probes: every result is inconclusive.
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_probe_executor(failing_canary(false)),
    )
    .unwrap();
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let entry = summary.entry(strategy, 1).unwrap();
        assert_eq!(entry.canary_status, Some(CanaryStatus::Inconclusive));
        assert_eq!(
            entry.adaptation.as_ref().unwrap().status,
            AdaptationStatus::Blocked
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Rejected candidate
// ---------------------------------------------------------------------------------------------

#[test]
fn rejected_model_output_keeps_the_prior_kb_and_records_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::scripted(vec![
        "this is not a knowledge base".to_owned(),
        "{\"ops\": [{\"op\": \"remove_card\", \"lineage_id\": \"tracker.list_issues\"}]}"
            .to_owned(),
    ])
    .with_identity("mock", "mock-1");
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_provider(Box::new(provider)),
    )
    .unwrap();
    let events = read_run_events(dir.path());

    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let o = outcome_of(&events, strategy);
        assert_eq!(
            o.status,
            AdaptationStatus::Rejected,
            "{strategy}: {}",
            o.reason
        );
        assert_eq!(o.candidate_hash, None);
        // Error event recorded, non-fatal; no canary ran for a candidate that never existed.
        let errors: Vec<_> = events
            .iter()
            .filter(|e| e.strategy == Some(strategy))
            .filter_map(|e| match &e.body {
                EventBody::Error(err) => Some(err),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "{strategy}");
        assert!(!errors[0].fatal);
        assert_eq!(count_type(&events, strategy, "canary_result"), 0);

        let bytes = std::fs::read(
            dir.path()
                .join(format!("kb/{}-rep1.json", strategy.as_str())),
        )
        .unwrap();
        assert_eq!(
            hex_sha256(&bytes),
            o.prior_hash,
            "prior KB kept for the runner"
        );
        let entry = summary.entry(strategy, 1).unwrap();
        assert_eq!(entry.canary_status, None);
        assert_eq!(entry.stage, StageStatus::Completed);
        assert_eq!(entry.errors.len(), 1);
    }
    assert_eq!(final_status(&events).0, RunStatus::Ok);
}

#[test]
fn provider_failure_is_recorded_and_makes_the_run_failed() {
    let dir = tempfile::tempdir().unwrap();
    // An empty script and no fixture responses: every call fails inside the provider.
    let provider = MockProvider::scripted(Vec::new()).with_identity("mock", "mock-1");
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_provider(Box::new(provider)),
    )
    .unwrap();
    assert_eq!(summary.status, RunStatus::Failed, "{summary}");
    let entry = summary.entry(Strategy::FullRewrite, 1).unwrap();
    assert_eq!(
        entry.adaptation.as_ref().unwrap().status,
        AdaptationStatus::Blocked
    );
    // The matrix went on and recorded the other strategies.
    assert_eq!(summary.entries.len(), 3);
}

// ---------------------------------------------------------------------------------------------
// Runner failure
// ---------------------------------------------------------------------------------------------

#[test]
fn runner_child_failure_is_recorded_and_the_run_continues_but_ends_failed() {
    let dir = tempfile::tempdir().unwrap();
    let runner =
        FixtureRunner::new(repo_root().join("fixtures/smoke/runner")).with_simulated_exit(1);
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_runner(Box::new(runner)),
    )
    .unwrap();
    let events = read_run_events(dir.path());

    assert_eq!(summary.status, RunStatus::Failed);
    assert_eq!(
        summary.entries.len(),
        3,
        "the run continues to record every strategy"
    );
    for entry in &summary.entries {
        assert_eq!(entry.stage, StageStatus::Failed);
        assert!(
            entry
                .errors
                .iter()
                .any(|e| e.contains("runner stage failed")),
            "{:?}",
            entry.errors
        );
        // Partial events were still forwarded.
        assert!(
            entry.verdicts.confirmed_success
                + entry.verdicts.confirmed_failure
                + entry.verdicts.inconclusive
                > 0
        );
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.body, EventBody::Error(err) if !err.fatal))
    );
    let (status, detail) = final_status(&events);
    assert_eq!(status, RunStatus::Failed);
    assert!(detail.unwrap().contains("runner stage failed"));
}

#[test]
fn runner_budget_exit_stops_the_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let runner =
        FixtureRunner::new(repo_root().join("fixtures/smoke/runner")).with_simulated_exit(2);
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_runner(Box::new(runner)),
    )
    .unwrap();
    assert_eq!(summary.status, RunStatus::BudgetExhausted);
    assert_eq!(summary.entries.len(), 1, "stops after the first stage");
    assert_eq!(summary.entries[0].stage, StageStatus::BudgetExhausted);
}

#[test]
fn a_bad_runner_log_is_a_recorded_failure_not_silent_success() {
    let dir = tempfile::tempdir().unwrap();
    let runner_dir = dir.path().join("runner");
    copy_dir(&repo_root().join("fixtures/smoke/runner"), &runner_dir);
    let path = runner_dir.join("selective_patch.jsonl");
    let mut text = read_text(&path);
    text.push_str("{\"schema_version\":\"run-event/2\"}\n");
    std::fs::write(&path, text).unwrap();
    let mut config = smoke_config();
    config.runner.fixture_dir = Some(runner_dir);

    let out = dir.path().join("out");
    let summary = run_matrix(&config, opts(&out)).unwrap();
    assert_eq!(summary.status, RunStatus::Failed);
    let entry = summary.entry(Strategy::SelectivePatch, 1).unwrap();
    assert_eq!(entry.stage, StageStatus::Failed);
    assert!(
        entry.errors.iter().any(|e| e.contains("line")),
        "{:?}",
        entry.errors
    );
    // Everything in the merged log is still schema-valid.
    read_run_events(&out);
}

// ---------------------------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------------------------

fn paid_mock(without_usage: bool) -> Box<MockProvider> {
    let provider = MockProvider::from_fixture_dir(repo_root().join("fixtures/smoke/provider"))
        .unwrap()
        .with_kind(ProviderKind::Paid)
        .with_identity("openai", "gpt-x");
    Box::new(if without_usage {
        provider.without_usage()
    } else {
        provider
    })
}

fn rate_card(input_per_mtok: f64, output_per_mtok: f64) -> RateCard {
    RateCard {
        id: "test-rates".to_owned(),
        date: "2026-01-01".to_owned(),
        entries: vec![RateEntry {
            provider: "openai".to_owned(),
            model: "gpt-x".to_owned(),
            input_usd_per_mtok: input_per_mtok,
            output_usd_per_mtok: output_per_mtok,
            cached_input_usd_per_mtok: None,
            cache_write_usd_per_mtok: None,
            long_context_threshold_tokens: None,
            long_input_usd_per_mtok: None,
            long_cached_input_usd_per_mtok: None,
            long_cache_write_usd_per_mtok: None,
            long_output_usd_per_mtok: None,
            price_date: None,
        }],
    }
}

#[test]
fn budget_refusal_stops_before_the_runner_stage_and_calls_no_provider() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = smoke_config();
    config.budget.cap_usd = 1e-9;
    config.budget.rate_card = Some(rate_card(1000.0, 1000.0));
    let summary = run_matrix(&config, opts(dir.path()).with_provider(paid_mock(false))).unwrap();
    let events = read_run_events(dir.path());

    assert_eq!(summary.status, RunStatus::BudgetExhausted, "{summary}");
    // no_update never calls the provider and completes; full_rewrite is refused.
    assert_eq!(summary.entries.len(), 2);
    assert_eq!(
        summary.entry(Strategy::NoUpdate, 1).unwrap().stage,
        StageStatus::Completed
    );
    let refused = summary.entry(Strategy::FullRewrite, 1).unwrap();
    assert_eq!(
        refused.stage,
        StageStatus::NotRun,
        "no runner stage after a refusal"
    );
    let o = outcome_of(&events, Strategy::FullRewrite);
    assert_eq!(o.status, AdaptationStatus::Blocked);
    assert!(o.reason.contains("local dispatch cap"), "{}", o.reason);
    assert_eq!(
        count_type(&events, Strategy::FullRewrite, "model_call"),
        0,
        "provider never called"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.body, EventBody::Error(err) if err.fatal))
    );
    assert!(!dir.path().join("python/full_rewrite-rep1.jsonl").exists());
    assert!(!dir.path().join("kb/full_rewrite-rep1.json").exists());
    assert_eq!(
        count_type(&events, Strategy::SelectivePatch, "config"),
        0,
        "matrix stopped"
    );
    assert_eq!(final_status(&events).0, RunStatus::BudgetExhausted);
}

#[test]
fn exhausted_budget_after_unknown_usage_stops_before_the_runner_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = smoke_config();
    // Input is free and output costs exactly $1 per token, so the reservation for one call
    // (8192 output tokens at the default limit) is exactly $8192. The provider reports no
    // usage, so that reservation stays held as unreconciled and consumes the whole cap.
    config.budget.cap_usd = 8192.0;
    config.budget.rate_card = Some(rate_card(0.0, 1_000_000.0));
    let summary = run_matrix(&config, opts(dir.path()).with_provider(paid_mock(true))).unwrap();
    let events = read_run_events(dir.path());

    assert_eq!(summary.status, RunStatus::BudgetExhausted, "{summary}");
    let entry = summary.entry(Strategy::FullRewrite, 1).unwrap();
    assert_eq!(
        entry.adaptation.as_ref().unwrap().status,
        AdaptationStatus::Promoted
    );
    assert_eq!(
        entry.stage,
        StageStatus::NotRun,
        "budget check happens before the runner stage"
    );
    assert!(!dir.path().join("python/full_rewrite-rep1.jsonl").exists());

    // Unknown usage: reported as unknown, never as zero USD.
    let call = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ModelCall(c) if e.strategy == Some(Strategy::FullRewrite) => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(call.cost_status, CostStatus::Unknown);
    assert_eq!(call.estimated_usd, None);
    assert_eq!(entry.cost.unknown_calls, 1);
    assert_eq!(entry.cost.priced_usd, None);
    assert_eq!(entry.cost.remaining_usd, 0.0);
    assert!(entry.cost.unreconciled_usd > 0.0);
    assert!(
        entry.cost.describe().contains("USD unknown"),
        "{}",
        entry.cost.describe()
    );
}

#[test]
fn remaining_budget_is_passed_to_the_runner_stage() {
    // The fixture runner ignores it, so use a recording runner.
    use harness_core::runner::{
        BenchmarkRunner, ExitInfo, RunnerError, StageOutcome, StageRequest,
    };
    use std::sync::Mutex;

    struct Recording(Arc<Mutex<Vec<StageRequest>>>);
    impl BenchmarkRunner for Recording {
        fn run_stage(
            &mut self,
            req: &StageRequest,
            _on_event: &mut dyn FnMut(RunEvent),
        ) -> Result<StageOutcome, RunnerError> {
            self.0.lock().unwrap().push(req.clone());
            Ok(StageOutcome {
                exit: ExitInfo { code: Some(0) },
                events: 0,
                raw_path: req.output_jsonl.clone(),
                budget_exhausted: false,
                raw_redacted: false,
            })
        }
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    run_matrix(
        &smoke_config(),
        opts(dir.path()).with_runner(Box::new(Recording(Arc::clone(&requests)))),
    )
    .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for req in requests.iter() {
        assert_eq!(req.remaining_budget_usd, 100.0);
        assert_eq!(req.run_id, "smoke");
        assert_eq!(req.server_version, "late");
        assert!(req.mock);
        assert_eq!(req.judge_version, "mock-judge-1");
        assert!(req.tool_kb_path.starts_with(dir.path()));
        assert!(req.tool_kb_path.is_file());
    }
}

// ---------------------------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------------------------

#[test]
fn a_preset_cancel_flag_records_only_run_complete_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(true));
    let summary = run_matrix(&smoke_config(), opts(dir.path()).with_cancel(cancel)).unwrap();
    assert_eq!(summary.status, RunStatus::Cancelled);
    assert!(summary.entries.is_empty());
    let events = read_run_events(dir.path());
    assert_eq!(events.len(), 1);
    assert_eq!(final_status(&events).0, RunStatus::Cancelled);
}

#[test]
fn cancelling_between_steps_stops_the_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&cancel);
    // Cancel as soon as the first adaptation outcome is written.
    let sink = FnSink::new(move |event: &RunEvent| {
        if matches!(event.body, EventBody::AdaptationOutcome(_)) {
            trigger.store(true, Ordering::SeqCst);
        }
        Ok(())
    });
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path())
            .with_cancel(cancel)
            .with_extra_sink(Box::new(sink)),
    )
    .unwrap();
    let events = read_run_events(dir.path());
    assert_eq!(summary.status, RunStatus::Cancelled);
    assert_eq!(final_status(&events).0, RunStatus::Cancelled);
    assert_eq!(summary.entries.len(), 1, "later strategies never start");
    assert_eq!(
        summary.entries[0].stage,
        StageStatus::NotRun,
        "cancelled before the runner stage"
    );
    assert_eq!(count_type(&events, Strategy::FullRewrite, "config"), 0);
}

#[test]
fn a_failing_extra_sink_never_stops_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let sink = FnSink::new(|_: &RunEvent| Err(harness_core::EventError::Sink("closed".to_owned())));
    let summary = run_matrix(
        &smoke_config(),
        opts(dir.path()).with_extra_sink(Box::new(sink)),
    )
    .unwrap();
    assert_eq!(summary.status, RunStatus::Ok);
    assert!(!read_run_events(dir.path()).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------

const FAKE_KEY: &str = "zk-live-9f8e7d6c5b4a39281706";

fn key_env() -> EnvLookup {
    Arc::new(|name| (name == "SMOKE_TEST_API_KEY").then(|| FAKE_KEY.to_owned()))
}

/// Config whose mock provider names a key env var and whose runner log leaks that key.
fn leaky_setup(dir: &Path) -> harness_core::Config {
    let runner_dir = dir.join("runner");
    copy_dir(&repo_root().join("fixtures/smoke/runner"), &runner_dir);
    let path = runner_dir.join("selective_patch.jsonl");
    let text = read_text(&path);
    assert!(text.contains("created issue APOLLO-101"));
    std::fs::write(
        &path,
        text.replacen(
            "created issue APOLLO-101",
            &format!("created issue APOLLO-101 with token {FAKE_KEY}"),
            1,
        ),
    )
    .unwrap();
    let mut config = smoke_config();
    config.provider.api_key_env = Some("SMOKE_TEST_API_KEY".to_owned());
    config.runner.fixture_dir = Some(runner_dir);
    config
}

#[test]
fn secrets_never_reach_events_manifest_summary_or_logs() {
    let dir = tempfile::tempdir().unwrap();
    let config = leaky_setup(dir.path());
    let out = dir.path().join("out");
    let sink = VecSink::new();
    let seen = sink.clone();
    let summary = run_matrix(
        &config,
        opts(&out)
            .with_env(key_env())
            .with_extra_sink(Box::new(sink)),
    )
    .unwrap();
    assert_eq!(summary.status, RunStatus::Ok);

    // The runner really did try to leak the key: it was redacted, not absent.
    let events = read_text(out.join("events.jsonl"));
    assert!(
        events.contains("[REDACTED]"),
        "the leaked key must show as redacted"
    );
    let raw = read_text(out.join("python/selective_patch-rep1.jsonl"));
    assert!(raw.contains("[REDACTED]"));

    for file in all_files(&out) {
        let text = read_text(&file);
        assert!(
            !text.contains(FAKE_KEY),
            "secret found in {}",
            file.display()
        );
    }
    let sunk = serde_json::to_string(&seen.events()).unwrap();
    assert!(!sunk.contains(FAKE_KEY), "secret reached the extra sink");
    // The manifest names the variable, never the value.
    let manifest = read_text(out.join("manifest.toml"));
    assert!(manifest.contains("api_key_env = \"SMOKE_TEST_API_KEY\""));
    // Nor the human-readable summary or error text.
    assert!(!summary.to_string().contains(FAKE_KEY));
}

#[test]
fn a_secret_in_the_environment_of_a_mock_run_is_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = smoke_config();
    config.provider.kind = ConfigProviderKind::Mock;
    config.provider.api_key_env = Some("SMOKE_TEST_API_KEY".to_owned());
    run_matrix(&config, opts(dir.path()).with_env(key_env())).unwrap();
    for file in all_files(dir.path()) {
        assert!(!read_text(&file).contains(FAKE_KEY), "{}", file.display());
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------------------------
// Process runner through the pipeline (Unix: a temp #!/bin/sh script stands in for Python)
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn process_runner_gets_the_env_override_by_name_and_never_in_argv() {
    use harness_core::config::RunnerKind;
    use std::os::unix::fs::PermissionsExt;

    const SCRIPT: &str = r#"#!/bin/sh
rec="$1"; fixtures="$2"
shift 2
: > "$rec"
for a in "$@"; do printf '%s\n' "$a" >> "$rec"; done
[ -n "${SMOKE_TEST_API_KEY:-}" ] && echo "ENV:SMOKE_TEST_API_KEY=present" >> "$rec"
out=""; runid=""; kb=""; prev=""
for a in "$@"; do
  [ "$prev" = "--output-jsonl" ] && out="$a"
  [ "$prev" = "--run-id" ] && runid="$a"
  [ "$prev" = "--tool-kb" ] && kb="$a"
  prev="$a"
done
strategy=$(basename "$kb" | sed 's/-rep.*//')
sed "s/smoke-fixture/$runid/" "$fixtures/$strategy.jsonl" > "$out"
exit 0
"#;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("runner.sh");
    std::fs::write(&script, SCRIPT).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = dir.path().join("argv.txt");

    let mut config = smoke_config();
    config.provider.api_key_env = Some("SMOKE_TEST_API_KEY".to_owned());
    config.runner.kind = RunnerKind::Process;
    config.runner.fixture_dir = None;
    config.runner.executable = Some(script.to_string_lossy().into_owned());
    config.runner.fixed_args = vec![
        record.to_string_lossy().into_owned(),
        repo_root()
            .join("fixtures/smoke/runner")
            .to_string_lossy()
            .into_owned(),
    ];
    assert_eq!(config.validate(), vec![]);

    let out = dir.path().join("out");
    let summary = run_matrix(&config, opts(&out).with_env(key_env())).unwrap();
    assert_eq!(summary.status, RunStatus::Ok, "{summary}");
    for entry in &summary.entries {
        assert_eq!(entry.stage, StageStatus::Completed, "{:?}", entry.errors);
    }

    // The last stage's argv was recorded: no secret in argv, key present in the child's env.
    let recorded = read_text(&record);
    assert!(!recorded.contains(FAKE_KEY), "{recorded}");
    assert!(
        recorded.contains("ENV:SMOKE_TEST_API_KEY=present"),
        "{recorded}"
    );
    assert!(recorded.contains("--run-id\nsmoke\n"), "{recorded}");
    assert!(
        recorded.contains("--task-id\ntracker-create-high-priority\n"),
        "{recorded}"
    );
    assert!(recorded.contains("--mock"), "{recorded}");
    // Events came through the same path as with the fixture runner.
    let events = read_run_events(&out);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.body, EventBody::VerifierVerdict(_)))
            .count(),
        3
    );
    for file in all_files(&out) {
        assert!(!read_text(&file).contains(FAKE_KEY), "{}", file.display());
    }
}
