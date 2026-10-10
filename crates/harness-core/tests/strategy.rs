//! The three strategies and candidate validation, on synthetic KBs.

use std::sync::Arc;

use harness_core::diff::{KbDiff, diff_kbs, unescape_path_segment};
use harness_core::patch::{Patch, PatchOp};
use harness_core::provider::{CompletionRequest, MeteredProvider, MockProvider};
use harness_core::strategy::{
    AdaptationContext, Candidate, StrategyError, kb_hash, strategy_for, validate_candidate,
    validate_candidate_against_source,
};
use harness_core::{
    ChangeKind, CostLedger, EventEmitter, EventSource, FixedClock, Provenance, RateCard, Redactor,
    RunEvent, Strategy, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb, VecSink, canonical_json,
};
use serde_json::{Value, json};

fn card(lineage: &str) -> ToolCard {
    ToolCard {
        lineage_id: lineage.into(),
        server_id: "srv".into(),
        tool_id: format!("{lineage}_tool"),
        name: format!("{lineage}_tool"),
        server_version: "v1".into(),
        description: "Does a thing.".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "a": { "type": "string" }, "b": { "type": "integer" } },
            "required": ["a"]
        }),
        output_schema: Some(
            json!({ "type": "object", "properties": { "out": { "type": "string" } } }),
        ),
        usage_rules: vec!["Be careful.".into()],
        provenance: Provenance {
            release_id: "rel".into(),
            source_file: "x/v1.json".into(),
            source_pointer: None,
        },
    }
}

/// Copy of `c` as it would appear in the v2 catalog.
fn v2(mut c: ToolCard) -> ToolCard {
    c.server_version = "v2".into();
    c.provenance.source_file = "x/v2.json".into();
    c
}

fn kb(version: &str, cards: Vec<ToolCard>) -> ToolKb {
    ToolKb {
        schema_version: TOOL_KB_SCHEMA_VERSION.into(),
        kb_id: format!("kb-{version}"),
        release_id: "rel".into(),
        server_version: version.into(),
        cards,
    }
}

struct Run {
    result: Result<Candidate, StrategyError>,
    calls: usize,
    requests: Vec<CompletionRequest>,
    events: Vec<RunEvent>,
}

fn run(strategy: Strategy, source: &ToolKb, target: &ToolKb, responses: Vec<String>) -> Run {
    let diff = diff_kbs(source, target);
    let mut mock = MockProvider::scripted(responses);
    let mut ledger = CostLedger::new(10.0, RateCard::empty("none", "2026-01-01"));
    let sink = VecSink::new();
    let mut emitter = EventEmitter::new(
        "run-1",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::default(),
        Box::new(sink.clone()),
    );
    let result = {
        let mut metered =
            MeteredProvider::new(&mut mock, &mut ledger, &mut emitter, Some(strategy));
        let mut ctx = AdaptationContext {
            source,
            target,
            diff: &diff,
            provider: &mut metered,
        };
        strategy_for(strategy).adapt(&mut ctx)
    };
    Run {
        result,
        calls: mock.call_count(),
        requests: mock.requests().to_vec(),
        events: sink.events(),
    }
}

fn kb_text(kb: &ToolKb) -> String {
    serde_json::to_string(kb).unwrap()
}

fn patch_text(patch: &Patch) -> String {
    serde_json::to_string(patch).unwrap()
}

fn value_at(card: &ToolCard, path: &str) -> Value {
    let mut cur = serde_json::to_value(card).unwrap();
    for seg in path.split('.') {
        cur = cur
            .get(unescape_path_segment(seg))
            .cloned()
            .unwrap_or(Value::Null);
    }
    cur
}

/// A correct patch derived from the diff (a reference for what a good model returns).
fn derive_patch(diff: &KbDiff, target: &ToolKb) -> Patch {
    let find = |lineage: &str| {
        target
            .cards
            .iter()
            .find(|c| c.lineage_id == lineage)
            .cloned()
    };
    let mut ops = Vec::new();
    for c in &diff.changes {
        match (c.kind, c.field_path.as_deref()) {
            (ChangeKind::Added, None) => ops.push(PatchOp::AddCard {
                card: Box::new(find(&c.lineage_id).expect("added card in target")),
            }),
            (ChangeKind::Removed, None) => ops.push(PatchOp::RemoveCard {
                lineage_id: c.lineage_id.clone(),
            }),
            (ChangeKind::Removed, Some(p))
                if p.split('.').count() >= 2
                    && (p.starts_with("input_schema") || p.starts_with("output_schema")) =>
            {
                ops.push(PatchOp::RemoveField {
                    lineage_id: c.lineage_id.clone(),
                    field_path: p.to_owned(),
                });
            }
            (_, Some(p)) => ops.push(PatchOp::SetField {
                lineage_id: c.lineage_id.clone(),
                field_path: p.to_owned(),
                value: value_at(&find(&c.lineage_id).expect("card in target"), p),
            }),
            (_, None) => unreachable!("only added/removed are whole-card changes"),
        }
    }
    Patch { ops }
}

fn card_bytes(kb: &ToolKb, lineage: &str) -> String {
    canonical_json(
        kb.cards
            .iter()
            .find(|c| c.lineage_id == lineage)
            .expect("card present"),
    )
}

fn structural(c: &ToolCard) -> (String, String, String, String, Value, Option<Value>) {
    (
        c.lineage_id.clone(),
        c.server_id.clone(),
        c.tool_id.clone(),
        c.name.clone(),
        c.input_schema.clone(),
        c.output_schema.clone(),
    )
}

fn assert_structural_equal(candidate: &ToolKb, target: &ToolKb) {
    let mut a: Vec<_> = candidate.cards.iter().map(structural).collect();
    let mut b: Vec<_> = target.cards.iter().map(structural).collect();
    a.sort_by(|x, y| x.0.cmp(&y.0));
    b.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(a, b);
}

/// One change scenario: source has `keep` and `edit`; target applies the change.
struct Case {
    name: &'static str,
    target: ToolKb,
}

fn cases() -> Vec<Case> {
    let mk = |name, edit: Option<ToolCard>, extra: Option<ToolCard>| {
        let mut cards = vec![v2(card("keep"))];
        if let Some(e) = edit {
            cards.push(v2(e));
        }
        if let Some(x) = extra {
            cards.push(v2(x));
        }
        Case {
            name,
            target: kb("v2", cards),
        }
    };
    let edit = || card("edit");
    let mut out = vec![
        mk("tool added", Some(edit()), Some(card("new"))),
        mk("tool removed", None, None),
    ];
    let mut renamed = edit();
    renamed.tool_id = "edit_renamed".into();
    renamed.name = "edit_renamed".into();
    out.push(mk("tool renamed", Some(renamed), None));
    let mut p_add = edit();
    p_add.input_schema["properties"]["c"] = json!({ "type": "string" });
    p_add.input_schema["required"] = json!(["a", "c"]);
    out.push(mk("param added (required)", Some(p_add), None));
    let mut p_rm = edit();
    p_rm.input_schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("b");
    out.push(mk("param removed", Some(p_rm), None));
    let mut p_ty = edit();
    p_ty.input_schema["properties"]["b"] = json!({ "type": "string" });
    out.push(mk("param type changed", Some(p_ty), None));
    let mut p_req = edit();
    p_req.input_schema["required"] = json!(["a", "b"]);
    out.push(mk("required changed", Some(p_req), None));
    let mut desc = edit();
    desc.description = "Now does a different thing.".into();
    out.push(mk("description changed", Some(desc), None));
    let mut rules = edit();
    rules.usage_rules = vec!["Never call twice.".into()];
    out.push(mk("usage rules changed", Some(rules), None));
    let mut outs = edit();
    outs.output_schema = Some(json!({ "type": "array" }));
    out.push(mk("output schema changed", Some(outs), None));
    let mut outs_rm = edit();
    outs_rm.output_schema = None;
    out.push(mk("output schema removed", Some(outs_rm), None));
    out
}

fn source_kb() -> ToolKb {
    kb("v1", vec![card("keep"), card("edit")])
}

// ---------------------------------------------------------------------------------------------
// no update
// ---------------------------------------------------------------------------------------------

#[test]
fn no_update_returns_source_unchanged_without_provider_calls() {
    let source = source_kb();
    let target = &cases()[3].target;
    let r = run(Strategy::NoUpdate, &source, target, vec![]);
    let c = r.result.unwrap();
    assert_eq!(c.strategy, Strategy::NoUpdate);
    assert_eq!(c.kb, source);
    assert_eq!(c.kb.canonical_bytes(), source.canonical_bytes());
    assert_eq!(c.prior_hash, c.candidate_hash);
    assert_eq!(c.prior_hash, kb_hash(&source));
    assert!(c.changed_lineage_ids.is_empty() && c.changes.is_empty() && c.patch.is_none());
    assert_eq!(r.calls, 0);
    assert!(r.events.is_empty());
}

// ---------------------------------------------------------------------------------------------
// full rewrite
// ---------------------------------------------------------------------------------------------

#[test]
fn full_rewrite_covers_every_change_case() {
    let source = source_kb();
    for case in cases() {
        let r = run(
            Strategy::FullRewrite,
            &source,
            &case.target,
            vec![kb_text(&case.target)],
        );
        let c = r.result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(r.calls, 1, "{}", case.name);
        assert_structural_equal(&c.kb, &case.target);
        assert_eq!(c.kb.server_version, "v2");
        assert_eq!(c.prior_hash, kb_hash(&source));
        assert_ne!(c.candidate_hash, c.prior_hash, "{}", case.name);
        assert_eq!(c.candidate_hash, kb_hash(&c.kb));
        assert!(c.patch.is_none());
        let diff = diff_kbs(&source, &case.target);
        assert_eq!(c.changes, diff.changes);
        assert_eq!(
            c.changed_lineage_ids,
            diff.affected_lineage_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(r.requests[0].purpose, "full_rewrite");
        validate_candidate(&c.kb, &case.target, &diff, Strategy::FullRewrite).unwrap();
    }
}

#[test]
fn full_rewrite_stamps_harness_metadata_from_the_target() {
    let source = source_kb();
    let case = &cases()[7];
    // The model claims other metadata; the harness owns it.
    let mut lying = case.target.clone();
    lying.kb_id = "model-chosen".into();
    for c in &mut lying.cards {
        c.server_version = "v999".into();
        c.provenance.source_file = "model/made/up.json".into();
    }
    let c = run(
        Strategy::FullRewrite,
        &source,
        &case.target,
        vec![kb_text(&lying)],
    )
    .result
    .unwrap();
    assert_eq!(c.kb.kb_id, "kb-v2:full_rewrite");
    assert!(
        c.kb.cards
            .iter()
            .all(|k| k.server_version == "v2" && k.provenance.source_file == "x/v2.json")
    );
}

#[test]
fn full_rewrite_accepts_model_authored_text_changes() {
    let source = source_kb();
    let case = &cases()[7];
    let mut reworded = case.target.clone();
    reworded.cards[1].description = "A reworded but valid description.".into();
    let c = run(
        Strategy::FullRewrite,
        &source,
        &case.target,
        vec![kb_text(&reworded)],
    )
    .result
    .unwrap();
    assert_eq!(
        c.kb.cards[1].description,
        "A reworded but valid description."
    );
}

#[test]
fn full_rewrite_prompt_is_deterministic_and_uses_only_target_definitions() {
    let source = source_kb();
    let target = &cases()[7].target;
    let a = run(
        Strategy::FullRewrite,
        &source,
        target,
        vec![kb_text(target)],
    );
    let b = run(
        Strategy::FullRewrite,
        &source,
        target,
        vec![kb_text(target)],
    );
    assert_eq!(a.requests, b.requests);
    let req = &a.requests[0];
    assert!(req.user.contains(&canonical_json(&target.sorted())));
    assert!(req.json_schema.is_some());
    assert!(req.max_output_tokens > 0);
}

fn rejects_full_rewrite(response: String, target: &ToolKb) -> StrategyError {
    let r = run(Strategy::FullRewrite, &source_kb(), target, vec![response]);
    assert_eq!(r.calls, 1);
    r.result.expect_err("must be rejected")
}

#[test]
fn full_rewrite_rejects_invalid_or_incomplete_output() {
    let target = cases()[7].target.clone(); // description changed on `edit`
    let good = kb_text(&target);

    let errors = [
        rejects_full_rewrite(String::new(), &target),
        rejects_full_rewrite(format!("```json\n{good}\n```"), &target),
        rejects_full_rewrite("{\"schema_version\":\"tool-kb/1\"}".into(), &target),
        rejects_full_rewrite("[1,2,3]".into(), &target),
    ];
    for e in &errors {
        assert!(matches!(e, StrategyError::Output(_)), "{e:?}");
        assert!(e.is_rejection());
    }

    // Wrong schema version fails the schema.
    let mut wrong_version: Value = serde_json::from_str(&good).unwrap();
    wrong_version["schema_version"] = json!("tool-kb/9");
    assert!(matches!(
        rejects_full_rewrite(wrong_version.to_string(), &target),
        StrategyError::Output(_)
    ));

    // Missing card (incomplete coverage).
    let mut missing = target.clone();
    missing.cards.retain(|c| c.lineage_id != "edit");
    let e = rejects_full_rewrite(kb_text(&missing), &target);
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("missing"))),
        "{e:?}"
    );

    // Card for a tool that does not exist in the target.
    let mut extra = target.clone();
    extra.cards.push(card("ghost"));
    assert!(matches!(
        rejects_full_rewrite(kb_text(&extra), &target),
        StrategyError::Validation { .. }
    ));

    // Duplicate card.
    let mut dup = target.clone();
    dup.cards.push(dup.cards[0].clone());
    assert!(matches!(
        rejects_full_rewrite(kb_text(&dup), &target),
        StrategyError::Validation { .. }
    ));

    // Structural drift from the target definition.
    let mut drift = target.clone();
    drift.cards[1].input_schema["properties"]["b"] = json!({ "type": "boolean" });
    let e = rejects_full_rewrite(kb_text(&drift), &target);
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("input_schema"))),
        "{e:?}"
    );

    // A detected description change that was not applied (model returned the stale text).
    let mut stale = target.clone();
    stale.cards[1].description = "Does a thing.".into();
    let e = rejects_full_rewrite(kb_text(&stale), &target);
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("not applied"))),
        "{e:?}"
    );
}

#[test]
fn full_rewrite_never_returns_a_candidate_from_rejected_output() {
    let target = cases()[7].target.clone();
    let r = run(
        Strategy::FullRewrite,
        &source_kb(),
        &target,
        vec!["nonsense".into()],
    );
    assert!(r.result.is_err());
    // The call was made and metered; the output was still rejected.
    assert_eq!(r.events.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// selective patch
// ---------------------------------------------------------------------------------------------

#[test]
fn selective_patch_covers_every_change_case_and_keeps_unrelated_cards_byte_identical() {
    let source = source_kb();
    for case in cases() {
        let diff = diff_kbs(&source, &case.target);
        assert!(!diff.affected_lineage_ids.contains("keep"), "{}", case.name);
        let patch = derive_patch(&diff, &case.target);
        let r = run(
            Strategy::SelectivePatch,
            &source,
            &case.target,
            vec![patch_text(&patch)],
        );
        let c = r.result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(r.calls, 1, "{}", case.name);
        assert_eq!(r.requests[0].purpose, "selective_patch");

        // Unaffected card: byte-identical to the source, including v1 metadata.
        assert_eq!(
            card_bytes(&c.kb, "keep"),
            card_bytes(&source, "keep"),
            "{}",
            case.name
        );
        // Structure equals the target.
        assert_structural_equal(&c.kb, &case.target);
        assert_eq!(c.kb.server_version, "v2");
        assert_eq!(c.patch.as_ref(), Some(&patch));
        assert_eq!(c.prior_hash, kb_hash(&source));
        assert_eq!(c.candidate_hash, kb_hash(&c.kb));
        assert_ne!(c.candidate_hash, c.prior_hash);
        assert_eq!(c.changes, diff.changes);
        assert_eq!(
            c.changed_lineage_ids,
            diff.affected_lineage_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        );
        validate_candidate_against_source(
            &source,
            &c.kb,
            &case.target,
            &diff,
            Strategy::SelectivePatch,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        // Updated card carries target metadata; unaffected cards keep the source metadata.
        for k in &c.kb.cards {
            let expected = if diff.affected_lineage_ids.contains(&k.lineage_id) {
                "v2"
            } else {
                "v1"
            };
            assert_eq!(k.server_version, expected, "{} {}", case.name, k.lineage_id);
        }
    }
}

#[test]
fn selective_patch_prompt_lists_only_affected_cards_and_is_deterministic() {
    let source = source_kb();
    let case = &cases()[7];
    let diff = diff_kbs(&source, &case.target);
    let text = patch_text(&derive_patch(&diff, &case.target));
    let a = run(
        Strategy::SelectivePatch,
        &source,
        &case.target,
        vec![text.clone()],
    );
    let b = run(Strategy::SelectivePatch, &source, &case.target, vec![text]);
    assert_eq!(a.requests, b.requests);
    let user = &a.requests[0].user;
    assert!(user.contains("edit"));
    assert!(
        !user.contains("keep_tool"),
        "unaffected card must not be sent: {user}"
    );
    assert!(user.contains("input_schema") || user.contains("description"));
}

#[test]
fn selective_patch_with_no_changes_makes_no_provider_call() {
    let source = source_kb();
    let mut target = source.clone();
    target.server_version = "v2".into();
    for c in &mut target.cards {
        *c = v2(c.clone());
    }
    let r = run(Strategy::SelectivePatch, &source, &target, vec![]);
    let c = r.result.unwrap();
    assert_eq!(r.calls, 0);
    assert!(r.events.is_empty());
    assert_eq!(c.patch, Some(Patch::default()));
    assert!(c.changed_lineage_ids.is_empty());
    for lineage in ["keep", "edit"] {
        assert_eq!(card_bytes(&c.kb, lineage), card_bytes(&source, lineage));
    }
}

fn selective_error(patch_or_text: String) -> (StrategyError, usize) {
    let case = &cases()[3]; // param added (required)
    let r = run(
        Strategy::SelectivePatch,
        &source_kb(),
        &case.target,
        vec![patch_or_text],
    );
    (r.result.expect_err("must be rejected"), r.calls)
}

fn good_ops() -> Vec<PatchOp> {
    let case = &cases()[3];
    derive_patch(&diff_kbs(&source_kb(), &case.target), &case.target).ops
}

fn set(lineage: &str, path: &str, value: Value) -> PatchOp {
    PatchOp::SetField {
        lineage_id: lineage.into(),
        field_path: path.into(),
        value,
    }
}

#[test]
fn selective_patch_rejects_malformed_model_output() {
    for text in [
        "",
        "not json",
        "```json\n{\"ops\":[]}\n```",
        "{\"ops\":\"x\"}",
        "{\"ops\":[{\"op\":\"nuke\"}]}",
    ] {
        let (e, calls) = selective_error(text.to_owned());
        assert!(matches!(e, StrategyError::Patch(_)), "{text}: {e:?}");
        assert!(e.is_rejection());
        assert_eq!(calls, 1);
    }
}

#[test]
fn selective_patch_rejects_edits_outside_the_diff() {
    // Touching the unaffected card.
    let mut ops = good_ops();
    ops.push(set("keep", "description", json!("sneaky")));
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    assert!(matches!(e, StrategyError::Patch(_)), "{e:?}");

    // Touching an unchanged field of the affected card.
    let mut ops = good_ops();
    ops.push(set("edit", "description", json!("unrequested rewrite")));
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    assert!(matches!(e, StrategyError::Patch(_)), "{e:?}");

    // Broadening to a parent path.
    let ops = vec![set("edit", "input_schema", json!({ "type": "object" }))];
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    assert!(matches!(e, StrategyError::Patch(_)), "{e:?}");
}

#[test]
fn selective_patch_rejects_incomplete_or_wrong_patches_without_repair() {
    // Empty patch although the diff is not empty.
    let (e, _) = selective_error(patch_text(&Patch::default()));
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("input_schema"))),
        "{e:?}"
    );

    // Only half of the change (new param without the required list).
    let ops: Vec<_> = good_ops().into_iter().take(1).collect();
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    assert!(matches!(e, StrategyError::Validation { .. }), "{e:?}");

    // Right path, wrong value.
    let ops = vec![
        set(
            "edit",
            "input_schema.properties.c",
            json!({ "type": "integer" }),
        ),
        set("edit", "input_schema.required", json!(["a", "c"])),
    ];
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    assert!(matches!(e, StrategyError::Validation { .. }), "{e:?}");

    // Patch that cannot be applied (invalid value type).
    let ops = vec![set("edit", "input_schema.required", json!("a"))];
    let (e, _) = selective_error(patch_text(&Patch { ops }));
    // `required` is not a string-typed field, so this applies but fails against the target.
    assert!(
        matches!(
            e,
            StrategyError::Validation { .. } | StrategyError::Patch(_)
        ),
        "{e:?}"
    );
}

#[test]
fn selective_patch_rejects_unapplied_description_change() {
    let case = &cases()[7]; // description changed
    let stale = vec![set("edit", "description", json!("Does a thing."))];
    let r = run(
        Strategy::SelectivePatch,
        &source_kb(),
        &case.target,
        vec![patch_text(&Patch { ops: stale })],
    );
    let e = r.result.expect_err("stale description must be rejected");
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("not applied"))),
        "{e:?}"
    );
}

#[test]
fn selective_patch_output_never_leaves_partial_state() {
    let (e, calls) = selective_error("garbage".into());
    assert!(e.is_rejection());
    assert_eq!(calls, 1);
}

// ---------------------------------------------------------------------------------------------
// provider failures and validate_candidate
// ---------------------------------------------------------------------------------------------

#[test]
fn provider_failure_is_not_a_rejection() {
    let source = source_kb();
    let target = &cases()[7].target;
    // Empty script: the mock errors on the first call.
    let r = run(Strategy::FullRewrite, &source, target, vec![]);
    let e = r.result.expect_err("provider error");
    assert!(matches!(e, StrategyError::Provider(_)));
    assert!(!e.is_rejection());
}

#[test]
fn strategy_for_returns_the_matching_strategy() {
    for s in Strategy::ALL {
        assert_eq!(strategy_for(s).strategy(), s);
    }
}

#[test]
fn validate_candidate_checks_schema_and_no_update_identity() {
    let source = source_kb();
    let target = cases()[7].target.clone();
    let diff = diff_kbs(&source, &target);

    // NoUpdate: schema only without a source, identity with one.
    validate_candidate(&source, &target, &diff, Strategy::NoUpdate).unwrap();
    validate_candidate_against_source(&source, &source, &target, &diff, Strategy::NoUpdate)
        .unwrap();
    let e = validate_candidate_against_source(&source, &target, &target, &diff, Strategy::NoUpdate)
        .expect_err("changed KB is not a no-update candidate");
    assert!(matches!(e, StrategyError::Validation { .. }));

    // Schema-invalid candidate.
    let mut broken = target.clone();
    broken.schema_version = "nope".into();
    assert!(validate_candidate(&broken, &target, &diff, Strategy::FullRewrite).is_err());
    let mut empty_name = target.clone();
    empty_name.cards[0].name.clear();
    assert!(validate_candidate(&empty_name, &target, &diff, Strategy::FullRewrite).is_err());
}

#[test]
fn validate_candidate_flags_removed_tools_and_source_drift() {
    let source = source_kb();
    let case = &cases()[1]; // `edit` removed
    let diff = diff_kbs(&source, &case.target);

    // A card for the removed tool must not exist.
    let with_removed = kb("v2", vec![v2(card("keep")), v2(card("edit"))]);
    let e =
        validate_candidate(&with_removed, &case.target, &diff, Strategy::FullRewrite).unwrap_err();
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("removed"))),
        "{e:?}"
    );

    // SelectivePatch: unaffected card modified (even only in metadata) is not byte-identical.
    let mut drifted = kb("v2", vec![v2(card("keep"))]);
    drifted.cards[0].server_version = "v9".into();
    let e = validate_candidate_against_source(
        &source,
        &drifted,
        &case.target,
        &diff,
        Strategy::SelectivePatch,
    )
    .unwrap_err();
    assert!(
        matches!(&e, StrategyError::Validation { problems } if problems.iter().any(|p| p.contains("byte-identical"))),
        "{e:?}"
    );

    // Unaffected card content differs from target: caught without the source too.
    let mut edited = kb("v2", vec![card("keep")]);
    edited.cards[0].description = "changed".into();
    edited.server_version = "v2".into();
    assert!(validate_candidate(&edited, &case.target, &diff, Strategy::SelectivePatch).is_err());
}
