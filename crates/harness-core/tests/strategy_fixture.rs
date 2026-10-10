//! FixtureAdapter and the three strategies end to end against `fixtures/smoke`.
//! MockProvider + MeteredProvider + CostLedger + EventEmitter(VecSink); no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::diff::{KbDiff, diff_kbs};
use harness_core::patch::{Patch, validate_patch};
use harness_core::provider::{MeteredProvider, MockProvider};
use harness_core::source::{FixtureAdapter, SourceAdapter};
use harness_core::strategy::{
    AdaptationContext, Candidate, kb_hash, strategy_for, validate_candidate,
    validate_candidate_against_source,
};
use harness_core::{
    ChangeClass, ChangeKind, CostLedger, CostStatus, EventBody, EventEmitter, EventSource,
    FixedClock, ModelCallEvent, Phase, RateCard, Redactor, RunEvent, Strategy, ToolKb,
    VersionStage, canonical_json, parse_event_line,
};

fn smoke_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke")
}

fn adapter() -> FixtureAdapter {
    FixtureAdapter::open(smoke_root()).expect("smoke fixture opens")
}

fn early_late() -> (ToolKb, ToolKb, KbDiff) {
    let a = adapter();
    let early = a.load_kb("early").unwrap();
    let late = a.load_kb("late").unwrap();
    let diff = diff_kbs(&early, &late);
    (early, late, diff)
}

// ---------------------------------------------------------------------------------------------
// FixtureAdapter
// ---------------------------------------------------------------------------------------------

#[test]
fn adapter_lists_one_server_three_versions_and_one_task() {
    let a = adapter();
    assert_eq!(a.release_id(), "smoke-release-0");
    assert_eq!(a.version_labels(), ["early", "middle", "late"]);
    let inv = a.inventory().unwrap();
    assert_eq!(inv.release_id, "smoke-release-0");
    assert_eq!(inv.servers.len(), 1);
    let server = &inv.servers[0];
    assert_eq!(server.server_id, "tracker");
    let versions: Vec<_> = server
        .versions
        .iter()
        .map(|v| (v.label.as_str(), v.stage, v.tool_count))
        .collect();
    assert_eq!(
        versions,
        [
            ("early", Some(VersionStage::Early), 4),
            ("middle", Some(VersionStage::Middle), 4),
            ("late", Some(VersionStage::Late), 5)
        ]
    );
    assert_eq!(inv.tool_count_latest(), 5);
    assert_eq!(inv.tasks.len(), 1);
    assert_eq!(inv.tasks[0].stage, VersionStage::Late);
    assert_eq!(inv.tasks[0].server_id, "tracker");
}

#[test]
fn adapter_load_kb_and_unknown_version() {
    let a = adapter();
    for label in ["early", "middle", "late"] {
        let kb = a.load_kb(label).unwrap();
        assert_eq!(kb.server_version, label);
        assert_eq!(kb.release_id, "smoke-release-0");
    }
    let err = a.load_kb("v99").unwrap_err().to_string();
    assert!(err.contains("v99") && err.contains("early"), "{err}");
}

#[test]
fn rename_keeps_lineage_across_versions() {
    let a = adapter();
    let tool_of = |v: &str| {
        a.load_kb(v)
            .unwrap()
            .cards
            .into_iter()
            .find(|c| c.lineage_id == "tracker.search")
            .map(|c| c.tool_id)
    };
    assert_eq!(tool_of("early").as_deref(), Some("search_issues"));
    assert_eq!(tool_of("middle").as_deref(), Some("find_issues"));
    assert_eq!(tool_of("late").as_deref(), Some("find_issues"));
}

#[test]
fn smoke_diff_early_to_late_has_the_required_change_mix() {
    let (_, _, diff) = early_late();
    let has = |lineage: &str, class: ChangeClass, kind: ChangeKind, path: Option<&str>| {
        diff.changes.iter().any(|c| {
            c.lineage_id == lineage
                && c.class == class
                && c.kind == kind
                && c.field_path.as_deref() == path
        })
    };
    assert!(has(
        "tracker.add_comment",
        ChangeClass::Tool,
        ChangeKind::Added,
        None
    ));
    assert!(has(
        "tracker.create_issue",
        ChangeClass::Parameter,
        ChangeKind::Added,
        Some("input_schema.properties.priority")
    ));
    assert!(has(
        "tracker.create_issue",
        ChangeClass::Parameter,
        ChangeKind::Modified,
        Some("input_schema.required")
    ));
    assert!(has(
        "tracker.list_issues",
        ChangeClass::Description,
        ChangeKind::Modified,
        Some("description")
    ));
    assert!(has(
        "tracker.search",
        ChangeClass::Tool,
        ChangeKind::Renamed,
        Some("tool_id")
    ));
    assert!(has(
        "tracker.search",
        ChangeClass::Tool,
        ChangeKind::Renamed,
        Some("name")
    ));
    assert!(
        !diff.affected_lineage_ids.contains("tracker.get_issue"),
        "unaffected tool"
    );
    assert_eq!(diff.affected_lineage_ids.len(), 4);
}

#[test]
fn smoke_diff_middle_stages() {
    let a = adapter();
    let (early, middle, late) = (
        a.load_kb("early").unwrap(),
        a.load_kb("middle").unwrap(),
        a.load_kb("late").unwrap(),
    );
    let em = diff_kbs(&early, &middle);
    let ids: Vec<_> = em.affected_lineage_ids.iter().map(String::as_str).collect();
    assert_eq!(ids, ["tracker.list_issues", "tracker.search"]);
    let ml = diff_kbs(&middle, &late);
    let ids: Vec<_> = ml.affected_lineage_ids.iter().map(String::as_str).collect();
    assert_eq!(ids, ["tracker.add_comment", "tracker.create_issue"]);
}

fn copy_fixture(to: &Path) {
    fs::create_dir_all(to.join("kb")).unwrap();
    for name in ["early", "middle", "late"] {
        fs::copy(
            smoke_root().join(format!("kb/{name}.json")),
            to.join(format!("kb/{name}.json")),
        )
        .unwrap();
    }
    fs::copy(smoke_root().join("tasks.json"), to.join("tasks.json")).unwrap();
}

#[test]
fn lineage_json_is_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    let early_path = dir.path().join("kb/early.json");
    let text = fs::read_to_string(&early_path)
        .unwrap()
        .replace("tracker.search", "tracker.wrong");
    fs::write(&early_path, text).unwrap();

    // Without a map the wrong id is used as written.
    let no_map = FixtureAdapter::open(dir.path()).unwrap();
    assert!(
        no_map
            .load_kb("early")
            .unwrap()
            .cards
            .iter()
            .any(|c| c.lineage_id == "tracker.wrong")
    );

    fs::copy(
        smoke_root().join("lineage.json"),
        dir.path().join("lineage.json"),
    )
    .unwrap();
    let mapped = FixtureAdapter::open(dir.path()).unwrap();
    let early = mapped.load_kb("early").unwrap();
    assert!(early.cards.iter().any(|c| c.lineage_id == "tracker.search"));
    assert!(early.cards.iter().all(|c| c.lineage_id != "tracker.wrong"));
}

#[test]
fn fixture_errors_fail_closed() {
    // Missing kb dir.
    let dir = tempfile::tempdir().unwrap();
    assert!(FixtureAdapter::open(dir.path()).is_err());

    // KB whose server_version does not match its file name.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    fs::copy(
        dir.path().join("kb/early.json"),
        dir.path().join("kb/oops.json"),
    )
    .unwrap();
    let err = FixtureAdapter::open(dir.path()).unwrap_err().to_string();
    assert!(err.contains("oops"), "{err}");

    // Missing tasks.json.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    fs::remove_file(dir.path().join("tasks.json")).unwrap();
    assert!(FixtureAdapter::open(dir.path()).is_err());

    // Task naming an unknown server.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    let t = fs::read_to_string(dir.path().join("tasks.json"))
        .unwrap()
        .replace("\"tracker\"", "\"ghost\"");
    fs::write(dir.path().join("tasks.json"), t).unwrap();
    assert!(
        FixtureAdapter::open(dir.path())
            .unwrap_err()
            .to_string()
            .contains("ghost")
    );

    // Duplicate task ids.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    let one: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("tasks.json")).unwrap()).unwrap();
    let dup = serde_json::json!([one[0].clone(), one[0].clone()]);
    fs::write(dir.path().join("tasks.json"), dup.to_string()).unwrap();
    assert!(FixtureAdapter::open(dir.path()).is_err());

    // Lineage entry that matches nothing.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    fs::write(
        dir.path().join("lineage.json"),
        r#"[{"server_id":"tracker","tool_id":"nope","from_version":"early","lineage_id":"x"}]"#,
    )
    .unwrap();
    assert!(FixtureAdapter::open(dir.path()).is_err());

    // Schema-invalid KB is rejected.
    let dir = tempfile::tempdir().unwrap();
    copy_fixture(dir.path());
    let p = dir.path().join("kb/late.json");
    let bad = fs::read_to_string(&p)
        .unwrap()
        .replace("\"tool-kb/1\"", "\"tool-kb/2\"");
    fs::write(&p, bad).unwrap();
    assert!(FixtureAdapter::open(dir.path()).is_err());
}

// ---------------------------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------------------------

struct E2e {
    candidate: Candidate,
    events: Vec<RunEvent>,
    ledger: CostLedger,
    calls: usize,
}

fn run_fixture(strategy: Strategy) -> E2e {
    let (early, late, diff) = early_late();
    let mut mock = MockProvider::from_fixture_dir(smoke_root().join("provider")).unwrap();
    let mut ledger = CostLedger::new(100.0, RateCard::empty("smoke-rates", "2026-01-01"));
    let sink = harness_core::VecSink::new();
    let mut emitter = EventEmitter::new(
        "smoke-run",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::default(),
        Box::new(sink.clone()),
    );
    let candidate = {
        let mut metered =
            MeteredProvider::new(&mut mock, &mut ledger, &mut emitter, Some(strategy));
        let mut ctx = AdaptationContext {
            source: &early,
            target: &late,
            diff: &diff,
            provider: &mut metered,
        };
        strategy_for(strategy)
            .adapt(&mut ctx)
            .expect("fixture strategy succeeds")
    };
    E2e {
        candidate,
        events: sink.events(),
        ledger,
        calls: mock.call_count(),
    }
}

fn model_calls(events: &[RunEvent]) -> Vec<(&RunEvent, &ModelCallEvent)> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ModelCall(m) => Some((e, m)),
            _ => None,
        })
        .collect()
}

fn card_bytes(kb: &ToolKb, lineage: &str) -> String {
    canonical_json(kb.cards.iter().find(|c| c.lineage_id == lineage).unwrap())
}

#[test]
fn e2e_no_update_is_identity_and_emits_no_model_call() {
    let (early, _, _) = early_late();
    let r = run_fixture(Strategy::NoUpdate);
    assert_eq!(r.candidate.kb, early);
    assert_eq!(r.candidate.candidate_hash, kb_hash(&early));
    assert_eq!(r.candidate.prior_hash, r.candidate.candidate_hash);
    assert_eq!(r.calls, 0);
    assert!(model_calls(&r.events).is_empty());
    assert!(r.events.is_empty());
    assert_eq!(r.ledger.totals_overall().calls, 0);
}

#[test]
fn e2e_full_rewrite_equals_target_on_structural_fields_and_emits_mock_event() {
    let (early, late, diff) = early_late();
    let r = run_fixture(Strategy::FullRewrite);
    assert_eq!(r.calls, 1);
    let c = &r.candidate;

    assert_eq!(c.kb.cards.len(), late.cards.len());
    for t in &late.cards {
        let got =
            c.kb.cards
                .iter()
                .find(|k| k.lineage_id == t.lineage_id)
                .expect("card present");
        assert_eq!(
            (
                &got.server_id,
                &got.tool_id,
                &got.name,
                &got.input_schema,
                &got.output_schema
            ),
            (
                &t.server_id,
                &t.tool_id,
                &t.name,
                &t.input_schema,
                &t.output_schema
            ),
            "{}",
            t.lineage_id
        );
    }
    assert_eq!(c.prior_hash, kb_hash(&early));
    assert_ne!(c.candidate_hash, c.prior_hash);
    validate_candidate(&c.kb, &late, &diff, Strategy::FullRewrite).unwrap();
    validate_candidate_against_source(&early, &c.kb, &late, &diff, Strategy::FullRewrite).unwrap();

    let calls = model_calls(&r.events);
    assert_eq!(calls.len(), 1);
    let (event, call) = calls[0];
    assert_eq!(event.strategy, Some(Strategy::FullRewrite));
    assert_eq!(call.cost_status, CostStatus::Mock);
    assert_eq!(call.phase, Phase::Adaptation);
    assert_eq!(call.provider, "mock");
    assert_eq!(call.estimated_usd, None);
    assert!(call.input_tokens.is_some() && call.output_tokens.is_some());
    assert_eq!(r.ledger.totals_overall().mock_calls, 1);
    assert_eq!(r.ledger.spent_usd(), 0.0);
}

#[test]
fn e2e_selective_patch_leaves_unaffected_cards_byte_identical_and_emits_mock_event() {
    let (early, late, diff) = early_late();
    let r = run_fixture(Strategy::SelectivePatch);
    assert_eq!(r.calls, 1);
    let c = &r.candidate;

    // The unaffected tool is byte-identical to the early KB.
    assert_eq!(
        card_bytes(&c.kb, "tracker.get_issue"),
        card_bytes(&early, "tracker.get_issue")
    );
    // Everything else matches the target definitions.
    for t in &late.cards {
        let got =
            c.kb.cards
                .iter()
                .find(|k| k.lineage_id == t.lineage_id)
                .expect("card present");
        assert_eq!(got.input_schema, t.input_schema, "{}", t.lineage_id);
        assert_eq!(got.tool_id, t.tool_id, "{}", t.lineage_id);
        assert_eq!(got.output_schema, t.output_schema, "{}", t.lineage_id);
    }
    assert_eq!(c.kb.cards.len(), 5);
    assert_eq!(c.changed_lineage_ids.len(), 4);
    assert_eq!(c.changes, diff.changes);
    let patch: &Patch = c.patch.as_ref().expect("patch recorded");
    assert_eq!(patch.ops.len(), 6);
    validate_patch(patch, &diff).unwrap();
    validate_candidate_against_source(&early, &c.kb, &late, &diff, Strategy::SelectivePatch)
        .unwrap();

    let calls = model_calls(&r.events);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.strategy, Some(Strategy::SelectivePatch));
    assert_eq!(calls[0].1.cost_status, CostStatus::Mock);
    assert_eq!(r.ledger.totals_overall().mock_calls, 1);
}

#[test]
fn e2e_events_are_schema_valid_jsonl() {
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let r = run_fixture(strategy);
        for (i, e) in r.events.iter().enumerate() {
            let line = e.to_json_line().unwrap();
            let back = parse_event_line(i + 1, &line).unwrap();
            assert_eq!(&back, e);
        }
    }
}

#[test]
fn e2e_llm_strategies_are_deterministic() {
    for strategy in [Strategy::FullRewrite, Strategy::SelectivePatch] {
        let a = run_fixture(strategy);
        let b = run_fixture(strategy);
        assert_eq!(a.candidate, b.candidate);
        assert_eq!(a.events, b.events);
    }
}

#[test]
fn provider_fixture_responses_pass_validation_against_the_late_kb() {
    let (early, late, diff) = early_late();

    // Raw full-rewrite response is itself a valid full-rewrite candidate.
    let rewrite = ToolKb::read(smoke_root().join("provider/full_rewrite.json")).unwrap();
    validate_candidate(&rewrite, &late, &diff, Strategy::FullRewrite).unwrap();

    // Raw patch response parses, stays inside the diff and, applied to early, reaches the late structure.
    let text = fs::read_to_string(smoke_root().join("provider/selective_patch.json")).unwrap();
    let patch = harness_core::patch::parse_patch(&text).unwrap();
    validate_patch(&patch, &diff).unwrap();
    let applied = harness_core::patch::apply_patch(&early, &patch).unwrap();
    assert!(
        diff_kbs(&applied, &late).is_empty(),
        "patched early KB has no content change left to late"
    );
}
