//! Canary gate: probe generation, execution safety, status aggregation and promotion.

use std::sync::Arc;

use harness_core::canary::{
    CanaryGate, Probe, ProbeError, ProbeExecutor, ProbeObservation, PromotionDecision,
    ScriptedExecutor, SimulatedTargetExecutor, decide_promotion, generate_probes, looks_mutating,
    minimal_arguments, pick_control,
};
use harness_core::{
    AdaptationStatus, CanaryStatus, ChangeClass, ChangeKind, ChangeRecord, EventBody, EventEmitter,
    EventSource, FixedClock, Provenance, Redactor, RunEvent, Strategy, ToolCard, ToolKb, VecSink,
};
use serde_json::{Value, json};

fn card(lineage: &str, tool_id: &str, schema: Value) -> ToolCard {
    ToolCard {
        lineage_id: lineage.into(),
        server_id: "srv".into(),
        tool_id: tool_id.into(),
        name: tool_id.into(),
        server_version: "late".into(),
        description: format!("does {tool_id}"),
        input_schema: schema,
        output_schema: None,
        usage_rules: vec![],
        provenance: Provenance {
            release_id: "rel".into(),
            source_file: "kb.json".into(),
            source_pointer: None,
        },
    }
}

fn kb(version: &str, cards: Vec<ToolCard>) -> ToolKb {
    ToolKb {
        schema_version: "tool-kb/1".into(),
        kb_id: format!("kb-{version}"),
        release_id: "rel".into(),
        server_version: version.into(),
        cards,
    }
}

fn change(lineage: &str, tool_id: &str, class: ChangeClass, kind: ChangeKind) -> ChangeRecord {
    ChangeRecord {
        lineage_id: lineage.into(),
        tool_id: tool_id.into(),
        class,
        kind,
        field_path: None,
        old_hash: None,
        new_hash: None,
    }
}

fn emitter() -> (EventEmitter, VecSink) {
    let sink = VecSink::new();
    let emitter = EventEmitter::new(
        "run-canary",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::default(),
        Box::new(sink.clone()),
    );
    (emitter, sink)
}

fn schema_with(required: &[&str], props: Value) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

/// Target: `search(query*, limit)`, `get_status()`.
fn target_kb() -> ToolKb {
    kb(
        "late",
        vec![
            card(
                "srv.search",
                "search",
                schema_with(
                    &["query", "scope"],
                    json!({
                        "query": {"type": "string"},
                        "scope": {"type": "string", "enum": ["all", "mine"]},
                        "limit": {"type": "integer"}
                    }),
                ),
            ),
            card(
                "srv.status",
                "get_status",
                schema_with(&["id"], json!({"id": {"type": "string"}})),
            ),
        ],
    )
}

fn prior_kb() -> ToolKb {
    kb(
        "early",
        vec![
            card(
                "srv.search",
                "search",
                schema_with(&["query"], json!({"query": {"type": "string"}})),
            ),
            card(
                "srv.status",
                "get_status",
                schema_with(&["id"], json!({"id": {"type": "string"}})),
            ),
        ],
    )
}

fn search_changes() -> Vec<ChangeRecord> {
    vec![change(
        "srv.search",
        "search",
        ChangeClass::Parameter,
        ChangeKind::Modified,
    )]
}

fn events_of(sink: &VecSink) -> Vec<RunEvent> {
    sink.events()
}

fn canary_rows(events: &[RunEvent]) -> Vec<&harness_core::CanaryResultEvent> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::CanaryResult(c) => Some(c),
            _ => None,
        })
        .collect()
}

// ----------------------------------------------------------------------------------------
// Passing, failing, inconclusive
// ----------------------------------------------------------------------------------------

#[test]
fn passing_canary_promotes_and_emits_action_then_result() {
    let target = target_kb();
    let candidate = target.clone(); // adaptation matched the target exactly
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let (mut em, sink) = emitter();

    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&target),
        &mut executor,
        &mut em,
        Strategy::SelectivePatch,
    )
    .unwrap();

    assert_eq!(report.status, CanaryStatus::Pass);
    assert!(report.promote);
    assert!(report.control_included);
    assert!(!report.collateral_damage);
    assert_eq!(
        report.results.len(),
        2,
        "one targeted probe plus one control"
    );

    match decide_promotion(&report, &prior_kb(), &candidate) {
        PromotionDecision::Promote(kb) => assert_eq!(kb, candidate),
        other => panic!("expected promote, got {other:?}"),
    }

    let events = events_of(&sink);
    let kinds: Vec<_> = events.iter().map(|e| e.body.event_type()).collect();
    assert_eq!(
        kinds,
        ["mcp_action", "canary_result", "mcp_action", "canary_result"]
    );
    assert!(events.iter().all(|e| e.source == EventSource::Rust));
    assert!(
        events
            .iter()
            .all(|e| e.strategy == Some(Strategy::SelectivePatch))
    );
}

#[test]
fn event_refs_point_at_real_mcp_action_seq_numbers() {
    let target = target_kb();
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let (mut em, sink) = emitter();
    // Unrelated events first so seq numbers do not start at zero.
    for _ in 0..3 {
        em.emit(
            None,
            EventBody::Error(harness_core::ErrorEvent {
                message: "noise".into(),
                fatal: false,
            }),
        )
        .unwrap();
    }
    let report = CanaryGate::run(
        &target,
        &search_changes(),
        Some(&target),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();

    let events = events_of(&sink);
    let rows = canary_rows(&events);
    assert_eq!(rows.len(), 2);
    for (row, result) in rows.iter().zip(&report.results) {
        assert_eq!(row.event_refs.len(), 1);
        let referenced = events
            .iter()
            .find(|e| e.seq == row.event_refs[0])
            .expect("referenced event exists");
        match &referenced.body {
            EventBody::McpAction(action) => {
                assert_eq!(Some(&action.tool_id), row.tool_id.as_ref());
                assert!(action.task_id.contains(row.probe_id.as_deref().unwrap()));
            }
            other => panic!("event_refs must point at an mcp_action, got {other:?}"),
        }
        assert_eq!(result.action_seq, Some(row.event_refs[0]));
    }
    assert!(events[3].seq >= 3);
}

#[test]
fn failing_canary_blocks_and_keeps_prior_kb() {
    let target = target_kb();
    // Stale candidate: missing the new required `scope` parameter.
    let candidate = prior_kb();
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let (mut em, sink) = emitter();

    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&target),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();

    assert_eq!(report.status, CanaryStatus::Fail);
    assert!(!report.promote);
    let failing = report
        .results
        .iter()
        .find(|r| r.status == CanaryStatus::Fail)
        .unwrap();
    assert!(failing.actual.as_deref().unwrap().contains("scope"));

    // The prior KB is kept even though a (different) candidate exists.
    let prior = prior_kb();
    match decide_promotion(&report, &prior, &candidate) {
        PromotionDecision::Blocked { reason, kept } => {
            assert_eq!(kept, prior);
            assert!(reason.contains("canary failed"), "{reason}");
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    let decision = decide_promotion(&report, &prior, &candidate);
    assert_eq!(decision.adaptation_status(), AdaptationStatus::Blocked);
    assert_eq!(decision.active_kb(), &prior);
    assert!(!events_of(&sink).is_empty());
}

#[test]
fn inconclusive_canary_blocks_and_keeps_prior_kb() {
    let candidate = target_kb();
    let mut executor = ScriptedExecutor::new(true)
        .with_default(ProbeObservation::inconclusive("server timed out"));
    let (mut em, sink) = emitter();

    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        None,
        &mut executor,
        &mut em,
        Strategy::SelectivePatch,
    )
    .unwrap();

    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.promote);
    let prior = prior_kb();
    let decision = decide_promotion(&report, &prior, &candidate);
    assert_eq!(decision.adaptation_status(), AdaptationStatus::Blocked);
    assert_eq!(decision.active_kb(), &prior, "prior KB kept");
    assert!(matches!(decision, PromotionDecision::Blocked { .. }));

    let events = events_of(&sink);
    assert!(
        canary_rows(&events)
            .iter()
            .all(|r| r.status == CanaryStatus::Inconclusive)
    );
}

#[test]
fn executor_error_is_inconclusive_not_pass() {
    let candidate = target_kb();
    let mut executor = ScriptedExecutor::new(true).with_default(ProbeObservation::pass("ok"));
    let probes = generate_probes(&candidate, &search_changes(), None);
    executor = executor.with_outcome(
        probes[0].probe_id.clone(),
        Err(ProbeError::Execution("connection refused".into())),
    );
    let (mut em, _sink) = emitter();
    let report =
        CanaryGate::run_probes(&probes, &mut executor, &mut em, Strategy::FullRewrite).unwrap();
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(
        report.results[0]
            .actual
            .as_deref()
            .unwrap()
            .contains("connection refused")
    );
    assert!(!report.promote);
}

#[test]
fn reset_failure_prevents_execution_and_is_inconclusive() {
    let candidate = target_kb();
    let mut executor = ScriptedExecutor::new(true).with_reset_error("cannot restore baseline");
    let (mut em, _sink) = emitter();
    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        None,
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    assert!(executor.executed.is_empty());
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(report.results.iter().all(|r| !r.executed));
}

#[test]
fn state_is_reset_before_every_probe() {
    let candidate = target_kb();
    let mut executor = ScriptedExecutor::new(true);
    let (mut em, _sink) = emitter();
    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&candidate),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    assert_eq!(report.results.len(), 2);
    assert_eq!(executor.resets, 2);
    assert_eq!(executor.executed.len(), 2);
}

// ----------------------------------------------------------------------------------------
// Isolation and mutating probes
// ----------------------------------------------------------------------------------------

#[test]
fn non_isolated_executor_never_executes_any_probe() {
    let candidate = target_kb();
    let mut executor = ScriptedExecutor::new(false);
    let (mut em, sink) = emitter();
    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&candidate),
        &mut executor,
        &mut em,
        Strategy::SelectivePatch,
    )
    .unwrap();

    assert!(executor.executed.is_empty(), "no probe may run");
    assert_eq!(executor.resets, 0);
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.promote);
    for result in &report.results {
        assert!(!result.executed);
        assert_eq!(result.status, CanaryStatus::Inconclusive);
        assert!(result.actual.as_deref().unwrap().contains("not isolated"));
        assert!(result.action_seq.is_none());
    }
    let events = events_of(&sink);
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.body, EventBody::McpAction(_))),
        "a probe that did not run must not look like a server call"
    );
    assert!(canary_rows(&events).iter().all(|r| r.event_refs.is_empty()));
}

#[test]
fn mutating_probe_never_runs_on_non_disposable_state() {
    let candidate = kb(
        "late",
        vec![card(
            "srv.delete",
            "delete_item",
            schema_with(&["id"], json!({"id": {"type": "string"}})),
        )],
    );
    let changes = vec![change(
        "srv.delete",
        "delete_item",
        ChangeClass::Tool,
        ChangeKind::Added,
    )];
    let probes = generate_probes(&candidate, &changes, None);
    assert_eq!(probes.len(), 1);
    assert!(probes[0].mutating, "delete_* is a mutating tool");

    let mut executor = ScriptedExecutor::new(false);
    let (mut em, _sink) = emitter();
    let report =
        CanaryGate::run_probes(&probes, &mut executor, &mut em, Strategy::FullRewrite).unwrap();
    assert!(executor.executed.is_empty());
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(
        report.results[0]
            .actual
            .as_deref()
            .unwrap()
            .contains("mutating")
    );

    // On an isolated (disposable, resettable) executor the same probe runs, after a reset.
    let mut isolated = ScriptedExecutor::new(true);
    let (mut em2, _sink2) = emitter();
    let report2 =
        CanaryGate::run_probes(&probes, &mut isolated, &mut em2, Strategy::FullRewrite).unwrap();
    assert_eq!(isolated.executed.len(), 1);
    assert_eq!(isolated.resets, 1);
    assert_eq!(report2.status, CanaryStatus::Pass);
}

#[test]
fn mutating_heuristic_reads_snake_and_camel_case_tokens() {
    let c = |id: &str| card("l", id, json!({}));
    assert!(looks_mutating(&c("create_user")));
    assert!(looks_mutating(&c("deleteItem")));
    assert!(looks_mutating(&c("orders.update")));
    assert!(!looks_mutating(&c("get_forecast")));
    assert!(!looks_mutating(&c("list_settings")));
}

// ----------------------------------------------------------------------------------------
// Control probe and collateral damage
// ----------------------------------------------------------------------------------------

#[test]
fn control_probe_detects_collateral_damage() {
    let target = target_kb();
    // Candidate handles the changed tool correctly but damaged the unchanged `get_status`
    // card (renamed its required parameter).
    let mut candidate = target.clone();
    candidate.cards[1].input_schema =
        schema_with(&["identifier"], json!({"identifier": {"type": "string"}}));
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let (mut em, sink) = emitter();

    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&target),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();

    let targeted = report.results.iter().find(|r| !r.is_control).unwrap();
    let control = report.results.iter().find(|r| r.is_control).unwrap();
    assert_eq!(targeted.status, CanaryStatus::Pass);
    assert_eq!(control.status, CanaryStatus::Fail);
    assert_eq!(
        report.status,
        CanaryStatus::Fail,
        "control failure is overall fail"
    );
    assert!(report.collateral_damage);
    assert!(!report.promote);
    assert!(report.summary().contains("collateral damage"));

    let events = events_of(&sink);
    let control_row = canary_rows(&events)
        .into_iter()
        .find(|r| r.is_control)
        .unwrap();
    assert_eq!(control_row.status, CanaryStatus::Fail);
    assert_eq!(control_row.tool_id.as_deref(), Some("get_status"));
}

#[test]
fn unknown_tool_in_target_fails() {
    let target = target_kb();
    let mut candidate = target.clone();
    candidate.cards[0].tool_id = "search_v0".into(); // renamed in candidate, target disagrees
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let (mut em, _sink) = emitter();
    let report = CanaryGate::run(
        &candidate,
        &search_changes(),
        Some(&target),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    assert_eq!(report.status, CanaryStatus::Fail);
    assert!(
        report.results[0]
            .actual
            .as_deref()
            .unwrap()
            .contains("no tool 'search_v0'")
    );
}

#[test]
fn simulated_target_checks_types_enums_and_unknown_arguments() {
    let target = target_kb();
    let mut executor = SimulatedTargetExecutor::from_target_kb(&target);
    let probe = |args: Value| Probe {
        probe_id: "p".into(),
        tool_id: "search".into(),
        lineage_id: "srv.search".into(),
        arguments: args.as_object().unwrap().clone(),
        expected: "accepted".into(),
        is_control: false,
        mutating: false,
    };
    let ok = executor
        .execute(&probe(json!({"query": "x", "scope": "all"})))
        .unwrap();
    assert!(ok.ok);
    for (args, needle) in [
        (json!({"query": 5, "scope": "all"}), "type string"),
        (json!({"query": "x", "scope": "everyone"}), "enum"),
        (
            json!({"query": "x", "scope": "all", "bogus": 1}),
            "unknown argument",
        ),
        (json!({"query": "x"}), "missing required argument 'scope'"),
    ] {
        let obs = executor.execute(&probe(args)).unwrap();
        assert!(!obs.ok && !obs.inconclusive);
        assert!(obs.actual.contains(needle), "{}: {needle}", obs.actual);
    }
}

// ----------------------------------------------------------------------------------------
// Probe generation
// ----------------------------------------------------------------------------------------

#[test]
fn generates_between_one_and_three_targeted_probes_plus_optional_control() {
    let mut cards = Vec::new();
    let mut changes = Vec::new();
    for i in 0..5 {
        let lineage = format!("srv.tool{i}");
        cards.push(card(
            &lineage,
            &format!("tool_{i}"),
            schema_with(&["a"], json!({"a": {"type": "string"}})),
        ));
        changes.push(change(
            &lineage,
            &format!("tool_{i}"),
            ChangeClass::Parameter,
            ChangeKind::Modified,
        ));
    }
    cards.push(card(
        "srv.stable",
        "stable_tool",
        schema_with(&[], json!({})),
    ));
    let candidate = kb("late", cards);

    // Five changed tools: capped at three targeted probes.
    let control = pick_control(&candidate, &changes, None).unwrap();
    assert_eq!(control.lineage_id, "srv.stable");
    let probes = generate_probes(&candidate, &changes, Some(control));
    let targeted: Vec<_> = probes.iter().filter(|p| !p.is_control).collect();
    let controls: Vec<_> = probes.iter().filter(|p| p.is_control).collect();
    assert_eq!(targeted.len(), 3);
    assert_eq!(controls.len(), 1);
    assert_eq!(controls[0].tool_id, "stable_tool");

    // Without a control card: targeted only.
    assert_eq!(
        generate_probes(&candidate, &changes, None)
            .iter()
            .filter(|p| p.is_control)
            .count(),
        0
    );
    // A single change: exactly one targeted probe.
    let one = generate_probes(&candidate, &changes[..1], None);
    assert_eq!(one.len(), 1);
    assert!(!one[0].is_control);
}

#[test]
fn control_that_is_itself_changed_is_ignored() {
    let candidate = target_kb();
    let changed_card = &candidate.cards[0]; // srv.search is in the change list
    let probes = generate_probes(&candidate, &search_changes(), Some(changed_card));
    assert_eq!(probes.iter().filter(|p| p.is_control).count(), 0);
}

#[test]
fn probes_are_built_from_the_candidate_card_schema() {
    let candidate = kb(
        "late",
        vec![card(
            "srv.search",
            "search",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "scope": {"type": "string", "enum": ["all", "mine"]},
                    "limit": {"type": "integer", "default": 10},
                    "verbose": {"type": "boolean"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "opts": {"type": "object", "properties": {"deep": {"type": "boolean"}}, "required": ["deep"]},
                    "count": {"type": "number", "examples": [2.5]},
                    "optional": {"type": "string"}
                },
                "required": ["query", "scope", "limit", "verbose", "tags", "opts", "count"]
            }),
        )],
    );
    let probes = generate_probes(&candidate, &search_changes(), None);
    assert_eq!(probes.len(), 1);
    let args = Value::Object(probes[0].arguments.clone());
    assert_eq!(
        args,
        json!({
            "query": "sample",
            "scope": "all",
            "limit": 10,
            "verbose": false,
            "tags": [],
            "opts": {"deep": false},
            "count": 2.5
        }),
        "required parameters only; enum/default/example preferred"
    );
    assert!(!args.as_object().unwrap().contains_key("optional"));
    assert_eq!(probes[0].tool_id, "search");
    assert_eq!(probes[0].lineage_id, "srv.search");
    assert!(!probes[0].expected.is_empty());
    assert_eq!(
        minimal_arguments(&candidate.cards[0].input_schema),
        *args.as_object().unwrap()
    );
}

#[test]
fn removed_tools_get_no_probe_and_no_targets_is_inconclusive() {
    let candidate = target_kb();
    let removed = vec![change(
        "srv.gone",
        "gone",
        ChangeClass::Tool,
        ChangeKind::Removed,
    )];
    assert!(generate_probes(&candidate, &removed, None).is_empty());

    let mut executor = ScriptedExecutor::new(true);
    let (mut em, sink) = emitter();
    let report = CanaryGate::run(
        &candidate,
        &removed,
        Some(&candidate),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.promote);
    assert!(executor.executed.is_empty());
    let events = events_of(&sink);
    let rows = canary_rows(&events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].probe_id, None);
}

#[test]
fn parameter_changes_are_probed_before_description_changes() {
    let candidate = kb(
        "late",
        vec![
            card("a.desc", "desc_tool", schema_with(&[], json!({}))),
            card("z.param", "param_tool", schema_with(&[], json!({}))),
            card("m.tool", "tool_tool", schema_with(&[], json!({}))),
            card("b.desc", "desc_two", schema_with(&[], json!({}))),
        ],
    );
    let changes = vec![
        change(
            "a.desc",
            "desc_tool",
            ChangeClass::Description,
            ChangeKind::Modified,
        ),
        change(
            "b.desc",
            "desc_two",
            ChangeClass::Description,
            ChangeKind::Modified,
        ),
        change("m.tool", "tool_tool", ChangeClass::Tool, ChangeKind::Added),
        change(
            "z.param",
            "param_tool",
            ChangeClass::Parameter,
            ChangeKind::Modified,
        ),
    ];
    let probes = generate_probes(&candidate, &changes, None);
    let tools: Vec<_> = probes.iter().map(|p| p.tool_id.as_str()).collect();
    assert_eq!(tools, ["param_tool", "tool_tool", "desc_tool"]);
}

// ----------------------------------------------------------------------------------------
// no-update and promotion
// ----------------------------------------------------------------------------------------

#[test]
fn no_update_records_not_applicable() {
    let (mut em, sink) = emitter();
    let report = CanaryGate::not_applicable(&mut em, Strategy::NoUpdate).unwrap();
    assert_eq!(report.status, CanaryStatus::NotApplicable);
    assert!(!report.promote);

    let events = events_of(&sink);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].strategy, Some(Strategy::NoUpdate));
    match &events[0].body {
        EventBody::CanaryResult(c) => {
            assert_eq!(c.status, CanaryStatus::NotApplicable);
            assert!(c.event_refs.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }

    let prior = prior_kb();
    let decision = decide_promotion(&report, &prior, &prior);
    assert_eq!(
        decision,
        PromotionDecision::Unchanged {
            kept: prior.clone()
        }
    );
    assert_eq!(decision.adaptation_status(), AdaptationStatus::Unchanged);
}

#[test]
fn promotion_requires_a_passing_status_even_if_flag_is_forged() {
    let target = target_kb();
    let mut executor = ScriptedExecutor::new(true).with_default(ProbeObservation::fail("rejected"));
    let (mut em, _sink) = emitter();
    let mut report = CanaryGate::run(
        &target,
        &search_changes(),
        None,
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    assert_eq!(report.status, CanaryStatus::Fail);
    report.promote = true; // forged flag must not promote a failed canary
    let decision = decide_promotion(&report, &prior_kb(), &target);
    assert!(matches!(decision, PromotionDecision::Blocked { .. }));
}

#[test]
fn secrets_in_probe_arguments_are_redacted_at_the_emitter() {
    let secret = "s3cr3t-value-123";
    let candidate = kb(
        "late",
        vec![card(
            "srv.auth",
            "auth",
            json!({"type": "object", "properties": {"token": {"type": "string", "default": secret}}, "required": ["token"]}),
        )],
    );
    let changes = vec![change(
        "srv.auth",
        "auth",
        ChangeClass::Parameter,
        ChangeKind::Modified,
    )];
    let sink = VecSink::new();
    let mut em = EventEmitter::new(
        "run-r",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::new(vec![secret.to_owned()]),
        Box::new(sink.clone()),
    );
    let mut executor = SimulatedTargetExecutor::from_target_kb(&candidate);
    CanaryGate::run(
        &candidate,
        &changes,
        Some(&candidate),
        &mut executor,
        &mut em,
        Strategy::FullRewrite,
    )
    .unwrap();
    let dump = serde_json::to_string(&sink.events()).unwrap();
    assert!(!dump.contains(secret));
}
