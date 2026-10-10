//! End-to-end smoke run of the pipeline: fixtures, mock provider, fixture runner, no network.

mod pipeline_common;

use std::collections::BTreeSet;
use std::path::Path;

use harness_core::pipeline::{RunSelection, RunSummary, run_matrix, validate_config_inputs};
use harness_core::strategy::kb_hash;
use harness_core::{
    AdaptationStatus, CanaryStatus, ConfigError, CostStatus, EventBody, EventSource, HarnessError,
    Phase, RunEvent, RunStatus, Strategy, VecSink, read_events,
};
use pipeline_common::{
    all_files, empty_env, opts, read_run_events, read_text, repo_root, smoke_config,
};
use sha2::{Digest, Sha256};

fn run_smoke(dir: &Path) -> RunSummary {
    run_matrix(&smoke_config(), opts(dir)).expect("smoke run")
}

fn of_strategy(events: &[RunEvent], strategy: Strategy) -> Vec<&RunEvent> {
    events
        .iter()
        .filter(|e| e.strategy == Some(strategy))
        .collect()
}

fn outcome(events: &[RunEvent], strategy: Strategy) -> harness_core::AdaptationOutcomeEvent {
    events
        .iter()
        .filter(|e| e.strategy == Some(strategy))
        .find_map(|e| match &e.body {
            EventBody::AdaptationOutcome(o) => Some(o.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no adaptation_outcome for {strategy}"))
}

#[test]
fn smoke_run_writes_the_complete_layout() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_smoke(dir.path());
    assert_eq!(summary.status, RunStatus::Ok, "{summary}");

    for path in [
        "manifest.toml",
        "events.jsonl",
        "summary.json",
        "kb/no_update-rep1.json",
        "kb/full_rewrite-rep1.json",
        "kb/selective_patch-rep1.json",
        "python/no_update-rep1.jsonl",
        "python/full_rewrite-rep1.jsonl",
        "python/selective_patch-rep1.jsonl",
    ] {
        assert!(dir.path().join(path).is_file(), "missing {path}");
    }
}

#[test]
fn every_event_passes_the_schema_and_all_strategies_are_present() {
    let dir = tempfile::tempdir().unwrap();
    run_smoke(dir.path());
    // read_events validates every line against the schema and fails on the first bad one.
    let events = read_run_events(dir.path());
    assert!(!events.is_empty());

    for strategy in Strategy::ALL {
        let mine = of_strategy(&events, strategy);
        let types: BTreeSet<&str> = mine.iter().map(|e| e.body.event_type()).collect();
        for needed in [
            "config",
            "adaptation_diff",
            "adaptation_outcome",
            "canary_result",
            "verifier_verdict",
            "metric_score",
            "model_call",
        ] {
            assert!(
                types.contains(needed),
                "{strategy}: missing {needed} in {types:?}"
            );
        }
    }

    // One final Rust run_complete without a strategy, and it is the last event.
    let last = events.last().unwrap();
    match &last.body {
        EventBody::RunComplete(done) => {
            assert_eq!(done.status, RunStatus::Ok);
            assert_eq!(last.strategy, None);
            assert_eq!(last.source, EventSource::Rust);
        }
        other => panic!("last event is {other:?}"),
    }
    let rust_completes = events
        .iter()
        .filter(|e| e.source == EventSource::Rust && matches!(e.body, EventBody::RunComplete(_)))
        .count();
    assert_eq!(rust_completes, 1);

    // Every event carries the run id, and seq is monotonic per source.
    for source in [EventSource::Rust, EventSource::Python] {
        let seqs: Vec<u64> = events
            .iter()
            .filter(|e| e.source == source)
            .map(|e| e.seq)
            .collect();
        assert!(!seqs.is_empty());
        assert_eq!(
            seqs,
            (0..seqs.len() as u64).collect::<Vec<_>>(),
            "{source:?}"
        );
    }
    assert!(events.iter().all(|e| e.run_id == "smoke"));
}

#[test]
fn adaptation_outcomes_and_canary_statuses_match_the_strategies() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_smoke(dir.path());
    let events = read_run_events(dir.path());

    assert_eq!(
        outcome(&events, Strategy::NoUpdate).status,
        AdaptationStatus::Unchanged
    );
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let o = outcome(&events, strategy);
        assert_eq!(
            o.status,
            AdaptationStatus::Promoted,
            "{strategy}: {}",
            o.reason
        );
        assert!(o.candidate_hash.is_some());
        assert!(!o.changed_lineage_ids.is_empty());
    }

    let canary = |strategy| -> Vec<CanaryStatus> {
        of_strategy(&events, strategy)
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::CanaryResult(c) => Some(c.status),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        canary(Strategy::NoUpdate),
        vec![CanaryStatus::NotApplicable]
    );
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let statuses = canary(strategy);
        assert!(!statuses.is_empty());
        assert!(
            statuses.iter().all(|s| *s == CanaryStatus::Pass),
            "{statuses:?}"
        );
    }

    let entry = |s| summary.entry(s, 1).expect("entry");
    assert_eq!(
        entry(Strategy::NoUpdate).canary_status,
        Some(CanaryStatus::NotApplicable)
    );
    assert_eq!(
        entry(Strategy::FullRewrite).canary_status,
        Some(CanaryStatus::Pass)
    );
    assert_eq!(
        entry(Strategy::SelectivePatch).canary_status,
        Some(CanaryStatus::Pass)
    );
}

#[test]
fn active_kb_files_are_canonical_and_match_the_outcome_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_smoke(dir.path());
    let events = read_run_events(dir.path());

    // no_update hands the runner the prior KB unchanged.
    let no_update = outcome(&events, Strategy::NoUpdate);
    let bytes = std::fs::read(dir.path().join("kb/no_update-rep1.json")).unwrap();
    assert_eq!(hex::encode(Sha256::digest(&bytes)), no_update.prior_hash);
    let early =
        harness_core::source::FixtureAdapter::open(repo_root().join("fixtures/smoke")).unwrap();
    use harness_core::source::SourceAdapter;
    assert_eq!(
        kb_hash(&early.load_kb("early").unwrap()),
        no_update.prior_hash
    );

    // Promoted strategies hand the runner the candidate.
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let o = outcome(&events, strategy);
        let bytes = std::fs::read(
            dir.path()
                .join(format!("kb/{}-rep1.json", strategy.as_str())),
        )
        .unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            o.candidate_hash.clone().unwrap()
        );
        assert_ne!(o.candidate_hash.unwrap(), o.prior_hash);
        // The file is a valid Tool KB whose version is the target.
        let kb = harness_core::ToolKb::from_json_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(kb.server_version, "late");
    }
    let entry = summary.entry(Strategy::SelectivePatch, 1).unwrap();
    assert_eq!(
        entry.active_kb_path.as_deref(),
        Some("kb/selective_patch-rep1.json")
    );
}

#[test]
fn verdicts_metrics_and_failures_are_passed_through_not_computed() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_smoke(dir.path());
    let events = read_run_events(dir.path());

    let no_update = summary.entry(Strategy::NoUpdate, 1).unwrap();
    assert_eq!(no_update.verdicts.confirmed_failure, 1);
    assert_eq!(no_update.failure_counts.under_update, 1);
    let selective = summary.entry(Strategy::SelectivePatch, 1).unwrap();
    assert_eq!(selective.verdicts.confirmed_success, 1);

    // Metric observations equal the metric_score events one to one.
    for strategy in Strategy::ALL {
        let from_events: Vec<(String, Option<String>, f64)> = of_strategy(&events, strategy)
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::MetricScore(m) => {
                    Some((format!("{:?}", m.metric), m.task_id.clone(), m.value))
                }
                _ => None,
            })
            .collect();
        let from_summary: Vec<(String, Option<String>, f64)> = summary
            .entry(strategy, 1)
            .unwrap()
            .metrics
            .iter()
            .map(|m| (format!("{:?}", m.metric), m.task_id.clone(), m.value))
            .collect();
        assert_eq!(from_events, from_summary, "{strategy}");
        assert_eq!(from_summary.len(), 5);
    }
}

#[test]
fn cost_counts_mock_calls_and_never_fabricates_usd() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_smoke(dir.path());
    let events = read_run_events(dir.path());

    let model_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ModelCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert!(!model_calls.is_empty());
    assert!(
        model_calls
            .iter()
            .all(|c| c.cost_status == CostStatus::Mock)
    );
    assert!(model_calls.iter().all(|c| c.estimated_usd.is_none()));
    // Both adaptation (rust) and agent (python) calls are present.
    assert!(model_calls.iter().any(|c| c.phase == Phase::Adaptation));
    assert!(model_calls.iter().any(|c| c.phase == Phase::Agent));

    let cost = &summary.cost;
    assert_eq!(
        cost.calls,
        model_calls.len() as u64,
        "python calls are ingested too"
    );
    assert_eq!(cost.mock_calls, cost.calls);
    assert_eq!(cost.priced_calls, 0);
    assert_eq!(cost.unknown_calls, 0);
    assert_eq!(
        cost.priced_usd, None,
        "no priced call: USD must be None, not 0"
    );
    assert_eq!(cost.local_dispatch_cap_usd, 100.0);
    assert_eq!(
        cost.remaining_usd, 100.0,
        "mock usage consumes no dispatch budget"
    );
    assert!(cost.by_phase.contains_key(&Phase::Adaptation));
    assert!(cost.by_phase.contains_key(&Phase::Agent));
    let per_entry: u64 = summary.entries.iter().map(|e| e.cost.calls).sum();
    assert_eq!(per_entry, cost.calls);
    assert!(summary.entry(Strategy::NoUpdate, 1).unwrap().cost.calls > 0);

    // The written summary carries the labels and the same numbers.
    let text = read_text(dir.path().join("summary.json"));
    assert!(text.contains("local_dispatch_cap_usd"));
    assert!(text.contains("not the provider's bill"));
    let parsed: RunSummary = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed.cost, summary.cost);
    assert_eq!(parsed.entries, summary.entries);
    let human = summary.to_string();
    assert!(human.contains("mock usage, not USD"), "{human}");
    assert!(human.contains("local dispatch cap"), "{human}");
}

#[test]
fn two_runs_are_byte_identical_with_a_fixed_clock() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    run_smoke(a.path());
    run_smoke(b.path());
    let files_a = all_files(a.path());
    let files_b = all_files(b.path());
    let names = |root: &Path, files: &[std::path::PathBuf]| -> Vec<String> {
        files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(names(a.path(), &files_a), names(b.path(), &files_b));
    for (fa, fb) in files_a.iter().zip(&files_b) {
        assert_eq!(
            std::fs::read(fa).unwrap(),
            std::fs::read(fb).unwrap(),
            "{} differs between runs",
            fa.display()
        );
    }
    let bytes = std::fs::read(a.path().join("events.jsonl")).unwrap();
    assert!(bytes.ends_with(b"\n"));
}

#[test]
fn manifest_records_names_checksums_and_contract_versions_only() {
    let dir = tempfile::tempdir().unwrap();
    run_smoke(dir.path());
    let text = read_text(dir.path().join("manifest.toml"));
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(manifest["contracts"]["tool_kb"].as_str(), Some("tool-kb/1"));
    assert_eq!(
        manifest["contracts"]["run_event"].as_str(),
        Some("run-event/1")
    );
    assert_eq!(manifest["run"]["name"].as_str(), Some("smoke"));
    assert_eq!(manifest["run"]["seed"].as_integer(), Some(1));
    assert_eq!(manifest["run"]["repetitions"].as_integer(), Some(1));
    let files = manifest["input_files"].as_array().unwrap();
    assert!(!files.is_empty());
    for f in files {
        let sha = f["sha256"].as_str().unwrap();
        assert_eq!(sha.len(), 64);
        assert!(
            !f["path"].as_str().unwrap().starts_with('/'),
            "names are relative"
        );
    }
    // Env var NAMES only; no absolute path of this machine leaks in.
    assert!(!text.contains(repo_root().to_str().unwrap()), "{text}");
}

#[test]
fn raw_python_logs_are_preserved_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    run_smoke(dir.path());
    for strategy in Strategy::ALL {
        let fixture = read_text(
            repo_root().join(format!("fixtures/smoke/runner/{}.jsonl", strategy.as_str())),
        );
        let kept = read_text(
            dir.path()
                .join(format!("python/{}-rep1.jsonl", strategy.as_str())),
        );
        assert_eq!(fixture.trim_end(), kept.trim_end(), "{strategy}");
    }
}

#[test]
fn extra_sink_receives_the_same_events_as_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let sink = VecSink::new();
    let handle = sink.clone();
    let tee_free: Box<dyn harness_core::EventSink + Send> = Box::new(sink);
    run_matrix(&smoke_config(), opts(dir.path()).with_extra_sink(tee_free)).unwrap();
    let from_file = read_events(dir.path().join("events.jsonl")).unwrap();
    assert_eq!(handle.events(), from_file);
}

#[test]
fn selection_overrides_run_a_subset() {
    let dir = tempfile::tempdir().unwrap();
    let selection = RunSelection {
        strategies: Some(vec![Strategy::SelectivePatch]),
        ..RunSelection::default()
    };
    let summary = run_matrix(&smoke_config(), opts(dir.path()).with_selection(selection)).unwrap();
    assert_eq!(summary.status, RunStatus::Ok);
    assert_eq!(summary.entries.len(), 1);
    assert_eq!(summary.entries[0].strategy, Strategy::SelectivePatch);
    assert!(!dir.path().join("kb/no_update-rep1.json").exists());
    let manifest = read_text(dir.path().join("manifest.toml"));
    assert!(
        manifest.contains("strategies = [\"selective_patch\"]"),
        "{manifest}"
    );
}

#[test]
fn repetitions_multiply_the_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = smoke_config();
    config.run.repetitions = 2;
    let summary = run_matrix(&config, opts(dir.path())).unwrap();
    assert_eq!(summary.entries.len(), 6);
    assert!(dir.path().join("kb/selective_patch-rep2.json").is_file());
    assert!(dir.path().join("python/no_update-rep2.jsonl").is_file());
    let events = read_run_events(dir.path());
    let configs: Vec<u32> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Config(c) => Some(c.repetition),
            _ => None,
        })
        .collect();
    assert_eq!(configs, vec![1, 1, 1, 2, 2, 2]);
}

#[test]
fn an_existing_run_directory_is_not_clobbered_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    run_smoke(dir.path());
    let before = std::fs::read(dir.path().join("events.jsonl")).unwrap();
    let err = run_matrix(&smoke_config(), opts(dir.path())).unwrap_err();
    assert!(err.to_string().contains("already holds a run"), "{err}");
    assert_eq!(
        std::fs::read(dir.path().join("events.jsonl")).unwrap(),
        before
    );

    // With overwrite the artifacts are replaced, not appended to.
    run_matrix(&smoke_config(), opts(dir.path()).with_overwrite(true)).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("events.jsonl")).unwrap(),
        before
    );
}

#[test]
fn validate_reports_counts_strategies_provider_and_schema_versions() {
    let report = validate_config_inputs(&smoke_config(), &*empty_env()).unwrap();
    assert_eq!(report.task_count(), 1);
    assert_eq!(report.servers.len(), 1);
    assert_eq!(report.strategies, Strategy::ALL.to_vec());
    assert_eq!(report.provider_model, "mock-1");
    assert_eq!(report.judge_version, "mock-judge-1");
    assert_eq!(report.tool_kb_schema_version, "tool-kb/1");
    assert_eq!(report.run_event_schema_version, "run-event/1");
    let tool_counts: Vec<usize> = report.servers[0]
        .versions
        .iter()
        .map(|v| v.tool_count)
        .collect();
    assert_eq!(tool_counts, vec![4, 4, 5]);
    assert!(report.detected_changes > 0);
    let text = report.to_string();
    for needle in [
        "1 selected",
        "mock",
        "no_update, full_rewrite, selective_patch",
        "tool-kb/1",
        "run-event/1",
        "smoke-mock-rates",
    ] {
        assert!(text.contains(needle), "{needle} not in\n{text}");
    }
}

#[test]
fn validate_lists_every_problem_at_once() {
    let mut config = smoke_config();
    config.tasks.ids = vec!["no-such-task".to_owned()];
    config.versions.target = "nope".to_owned();
    config.provider.mock_responses_dir = None;
    config.runner.fixture_dir = Some(repo_root().join("fixtures/does-not-exist"));
    let err = validate_config_inputs(&config, &*empty_env()).unwrap_err();
    let HarnessError::Config(ConfigError::Invalid { problems }) = err else {
        panic!("expected invalid config");
    };
    let fields: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
    for expected in [
        "tasks.ids",
        "versions.target",
        "provider.mock_responses_dir",
        "runner.fixture_dir",
    ] {
        assert!(fields.contains(&expected), "{expected} not in {fields:?}");
    }
}

#[test]
fn run_matrix_refuses_an_invalid_config_before_writing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut config = smoke_config();
    config.tasks.ids = vec!["ghost".to_owned()];
    let err = run_matrix(&config, opts(&out)).unwrap_err();
    assert!(
        matches!(err, HarnessError::Config(ConfigError::Invalid { .. })),
        "{err}"
    );
    assert!(
        !out.exists(),
        "nothing may be written for an invalid config"
    );
}
