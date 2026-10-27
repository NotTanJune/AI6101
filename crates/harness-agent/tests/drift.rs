//! Drift detection, the adaptation flow and explicit acceptance.

mod common;

use std::sync::Arc;

use common::{HarnessOpts, card, harness, kb};
use harness_agent::drift::{
    DriftError, SessionKb, detect_drift, evaluate_candidate, kb_subset, live_kb, prepare_candidate,
    run_adaptation,
};
use harness_agent::mcp::LiveTool;
use harness_agent::tools::{NamingMode, ToolRegistry};
use harness_core::canary::{PromotionDecision, SimulatedTargetExecutor};
use harness_core::events::{EventEmitter, EventSource, VecSink};
use harness_core::provider::{MeteredProvider, MockProvider};
use harness_core::{
    CanaryStatus, ChangeKind, CostLedger, FixedClock, RateCard, Redactor, Strategy, ToolKb,
};
use serde_json::Value;

fn live_tool(name: &str, description: &str) -> LiveTool {
    LiveTool {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: card("calc", name, "").input_schema,
        output_schema: None,
    }
}

fn session_kb() -> ToolKb {
    kb(vec![
        card("calc", "add", "Adds numbers."),
        card("calc", "legacy", "Old tool."),
        card("other", "untouched", "Belongs to another server."),
    ])
}

fn live_tools() -> Vec<LiveTool> {
    vec![
        live_tool("add", "Adds two integers and returns the sum."),
        live_tool("mul", "Multiplies two integers."),
    ]
}

/// Run `f` with a metered core provider backed by `mock`.
fn with_metered<T>(mock: &mut MockProvider, f: impl FnOnce(&mut MeteredProvider<'_>) -> T) -> T {
    let mut ledger = CostLedger::new(10.0, RateCard::empty("none", "2026-10-04"));
    let mut emitter = EventEmitter::new(
        "drift-test",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::default(),
        Box::new(VecSink::new()),
    );
    let mut metered = MeteredProvider::new(mock, &mut ledger, &mut emitter, None);
    f(&mut metered)
}

#[test]
fn drift_a_changed_description_yields_a_non_empty_diff() {
    let kb = session_kb();
    let (report, live) = detect_drift("calc", &kb, &live_tools());
    assert_eq!(report.server, "calc");
    assert_eq!(report.kb_cards, 2, "only the cards of this server");
    assert_eq!(report.live_tools, 2);
    assert!(!report.is_empty());

    let kinds: Vec<(String, ChangeKind, Option<String>)> = report
        .diff
        .changes
        .iter()
        .map(|c| (c.lineage_id.clone(), c.kind, c.field_path.clone()))
        .collect();
    assert!(
        kinds.contains(&(
            "calc::add".into(),
            ChangeKind::Modified,
            Some("description".into())
        )),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("live:mul".into(), ChangeKind::Added, None)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("calc::legacy".into(), ChangeKind::Removed, None)),
        "{kinds:?}"
    );
    // The matching tool keeps the KB lineage; the new one gets a live: lineage.
    let lineages: Vec<&str> = live.cards.iter().map(|c| c.lineage_id.as_str()).collect();
    assert_eq!(lineages, vec!["calc::add", "live:mul"]);
    assert_eq!(live.server_version, "live");
    assert!(live.cards.iter().all(|c| c.server_id == "calc"));
}

#[test]
fn drift_identical_definitions_give_an_empty_diff() {
    let kb = kb(vec![card("calc", "add", "Same text.")]);
    let live = vec![live_tool("add", "Same text.")];
    let (report, _) = detect_drift("calc", &kb, &live);
    assert!(report.is_empty(), "{:?}", report.diff.changes);

    // KB-only usage rules are not drift: a live listing has none.
    let mut with_rules = kb.clone();
    with_rules.cards[0].usage_rules = vec!["Call once.".into()];
    let (report, _) = detect_drift("calc", &with_rules, &live);
    assert!(report.is_empty(), "{:?}", report.diff.changes);
}

#[test]
fn drift_no_update_returns_the_original_kb_and_never_calls_the_provider() {
    let kb = session_kb();
    let subset = kb_subset(&kb, "calc");
    let (report, live) = detect_drift("calc", &kb, &live_tools());
    let mut mock = MockProvider::scripted(vec![]);
    let outcome = with_metered(&mut mock, |provider| {
        run_adaptation(Strategy::NoUpdate, &subset, &live, &report.diff, provider)
    })
    .expect("no update succeeds");
    assert_eq!(mock.call_count(), 0);
    assert_eq!(outcome.candidate.kb, subset);
    assert_eq!(outcome.canary.status, CanaryStatus::NotApplicable);
    assert!(matches!(
        outcome.decision,
        PromotionDecision::Unchanged { .. }
    ));

    // Even when the caller accepts the candidate, the KB is unchanged.
    let mut session = SessionKb::new(kb.clone());
    session.accept(&outcome.candidate);
    let mut ids_before: Vec<_> = kb
        .cards
        .iter()
        .map(|c| (&c.server_id, &c.tool_id, &c.description))
        .collect();
    let mut ids_after: Vec<_> = session
        .kb()
        .cards
        .iter()
        .map(|c| (&c.server_id, &c.tool_id, &c.description))
        .collect();
    ids_before.sort();
    ids_after.sort();
    assert_eq!(ids_before, ids_after);
}

fn exposed_description(session: &SessionKb, live: &[LiveTool]) -> String {
    let mut registry = ToolRegistry::new(NamingMode::Namespaced);
    registry
        .add_mcp_from_kb("calc", session.kb(), live)
        .expect("expose");
    registry
        .get("mcp__calc__add")
        .expect("add exposed")
        .description
        .clone()
}

#[test]
fn drift_accepting_a_candidate_changes_what_the_registry_exposes() {
    let kb = session_kb();
    let subset = kb_subset(&kb, "calc");
    let live_tools = live_tools();
    let (report, live) = detect_drift("calc", &kb, &live_tools);

    // The "model" answers with the live definitions as the rewritten KB.
    let rewritten = serde_json::to_string(&live).expect("serialise");
    let mut mock = MockProvider::scripted(vec![rewritten]);
    let outcome = with_metered(&mut mock, |provider| {
        let candidate = prepare_candidate(
            Strategy::FullRewrite,
            &subset,
            &live,
            &report.diff,
            provider,
        )?;
        let mut emitter = EventEmitter::new(
            "explicit-simulation-test",
            EventSource::Rust,
            Arc::new(FixedClock::default()),
            Redactor::default(),
            Box::new(VecSink::new()),
        );
        evaluate_candidate(
            Strategy::FullRewrite,
            &subset,
            &live,
            candidate,
            &mut SimulatedTargetExecutor::from_target_kb(&live),
            &mut emitter,
        )
    })
    .expect("full rewrite");
    assert_eq!(mock.call_count(), 1);
    assert_eq!(
        outcome.canary.status,
        CanaryStatus::Pass,
        "{}",
        outcome.canary.summary()
    );
    assert!(matches!(outcome.decision, PromotionDecision::Promote(_)));

    let mut session = SessionKb::new(kb.clone());
    // Nothing is accepted automatically: the registry still shows the KB description.
    assert_eq!(exposed_description(&session, &live_tools), "Adds numbers.");
    assert_eq!(session.kb(), &kb);

    let changed = session.accept(&outcome.candidate);
    assert!(changed >= 1);
    assert_eq!(
        exposed_description(&session, &live_tools),
        "Adds two integers and returns the sum."
    );
    // Cards of other servers survive the swap.
    assert!(session.kb().cards.iter().any(|c| c.server_id == "other"));
    // The candidate no longer drifts from the live server.
    let (after, _) = detect_drift("calc", session.kb(), &live_tools);
    assert!(after.is_empty(), "{:?}", after.diff.changes);
}

#[test]
fn drift_default_flow_never_substitutes_simulation_for_live_execution() {
    let kb = session_kb();
    let subset = kb_subset(&kb, "calc");
    let (report, live) = detect_drift("calc", &kb, &live_tools());
    let mut mock = MockProvider::scripted(vec![serde_json::to_string(&live).unwrap()]);
    let outcome = with_metered(&mut mock, |provider| {
        run_adaptation(
            Strategy::FullRewrite,
            &subset,
            &live,
            &report.diff,
            provider,
        )
    })
    .expect("candidate is prepared without probe permission");
    assert_eq!(outcome.canary.status, CanaryStatus::Inconclusive);
    assert!(outcome.canary.results.iter().all(|probe| !probe.executed));
    assert!(matches!(
        outcome.decision,
        PromotionDecision::Blocked { .. }
    ));
}

#[test]
fn drift_a_rejected_candidate_leaves_the_session_kb_alone() {
    let kb = session_kb();
    let subset = kb_subset(&kb, "calc");
    let (report, live) = detect_drift("calc", &kb, &live_tools());
    let mut mock = MockProvider::scripted(vec!["this is not JSON".to_owned()]);
    let err = with_metered(&mut mock, |provider| {
        run_adaptation(
            Strategy::FullRewrite,
            &subset,
            &live,
            &report.diff,
            provider,
        )
    })
    .expect_err("garbage output is rejected");
    assert!(matches!(err, DriftError::Strategy(_)), "{err}");
    let session = SessionKb::new(kb.clone());
    assert_eq!(session.kb(), &kb);
}

#[test]
fn drift_live_kb_uses_the_kb_lineage_only_for_matching_names() {
    let kb = session_kb();
    let live = live_kb("calc", &kb, &live_tools());
    assert_eq!(live.cards[0].lineage_id, "calc::add");
    assert_eq!(live.cards[1].lineage_id, "live:mul");
    assert_eq!(live.cards[1].usage_rules, Vec::<String>::new());
    let value: Value = serde_json::to_value(&live).expect("serialise");
    assert_eq!(value["schema_version"], "tool-kb/1");
    assert_eq!(value["kb_id"], "live:calc");
}

#[tokio::test]
async fn drift_agent_reports_drift_as_an_event() {
    let mut h = harness(HarnessOpts::new(vec![]));
    let kb = session_kb();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let report = h.agent.report_drift("calc", &kb, &live_tools(), &tx);
    drop(tx);
    assert!(!report.is_empty());
    match rx.recv().await {
        Some(harness_agent::events::AgentEvent::Drift(sent)) => assert_eq!(sent, report),
        other => panic!("expected a drift event, got {other:?}"),
    }
}
