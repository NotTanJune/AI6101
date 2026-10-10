//! Version diff and change classification.

use harness_core::diff::{KbDiff, diff_kbs, field_hash};
use harness_core::{
    ChangeClass, ChangeKind, ChangeRecord, Provenance, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb,
    card_hash,
};
use serde_json::{Value, json};

fn base_card(lineage: &str) -> ToolCard {
    ToolCard {
        lineage_id: lineage.into(),
        server_id: "srv".into(),
        tool_id: format!("{lineage}_tool"),
        name: format!("{lineage}_tool"),
        server_version: "v1".into(),
        description: "Does a thing.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "integer" }
            },
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

fn kb(version: &str, cards: Vec<ToolCard>) -> ToolKb {
    ToolKb {
        schema_version: TOOL_KB_SCHEMA_VERSION.into(),
        kb_id: format!("kb-{version}"),
        release_id: "rel".into(),
        server_version: version.into(),
        cards,
    }
}

fn only(diff: &KbDiff) -> &ChangeRecord {
    assert_eq!(diff.changes.len(), 1, "{:#?}", diff.changes);
    &diff.changes[0]
}

fn paths(diff: &KbDiff) -> Vec<(String, ChangeKind)> {
    diff.changes
        .iter()
        .map(|c| (c.field_path.clone().unwrap_or_default(), c.kind))
        .collect()
}

#[test]
fn identical_kbs_have_no_changes() {
    let a = kb("v1", vec![base_card("one"), base_card("two")]);
    let mut b = a.clone();
    b.server_version = "v2".into();
    // server_version and provenance of cards are never changes.
    for c in &mut b.cards {
        c.server_version = "v2".into();
        c.provenance.source_file = "x/v2.json".into();
    }
    let diff = diff_kbs(&a, &b);
    assert!(diff.is_empty());
    assert!(diff.affected_lineage_ids.is_empty());
    assert_eq!(diff.source_version, "v1");
    assert_eq!(diff.target_version, "v2");
}

#[test]
fn tool_added() {
    let a = kb("v1", vec![base_card("one")]);
    let new = base_card("two");
    let b = kb("v2", vec![base_card("one"), new.clone()]);
    let diff = diff_kbs(&a, &b);
    let c = only(&diff);
    assert_eq!(c.class, ChangeClass::Tool);
    assert_eq!(c.kind, ChangeKind::Added);
    assert_eq!(c.lineage_id, "two");
    assert_eq!(c.field_path, None);
    assert_eq!(c.old_hash, None);
    assert_eq!(c.new_hash.as_deref(), Some(card_hash(&new).as_str()));
    assert!(diff.is_added("two"));
    assert!(!diff.is_removed("two"));
    assert_eq!(
        diff.affected_lineage_ids.iter().collect::<Vec<_>>(),
        ["two"]
    );
}

#[test]
fn tool_removed() {
    let gone = base_card("two");
    let a = kb("v1", vec![base_card("one"), gone.clone()]);
    let b = kb("v2", vec![base_card("one")]);
    let diff = diff_kbs(&a, &b);
    let c = only(&diff);
    assert_eq!((c.class, c.kind), (ChangeClass::Tool, ChangeKind::Removed));
    assert_eq!(c.old_hash.as_deref(), Some(card_hash(&gone).as_str()));
    assert_eq!(c.new_hash, None);
    assert!(diff.is_removed("two"));
}

#[test]
fn rename_keeps_lineage() {
    let a = kb("v1", vec![base_card("one")]);
    let mut renamed = base_card("one");
    renamed.tool_id = "one_v2".into();
    renamed.name = "one_v2".into();
    let b = kb("v2", vec![renamed]);
    let diff = diff_kbs(&a, &b);
    assert_eq!(
        paths(&diff),
        vec![
            ("tool_id".to_owned(), ChangeKind::Renamed),
            ("name".to_owned(), ChangeKind::Renamed)
        ]
    );
    assert!(diff.changes.iter().all(|c| c.class == ChangeClass::Tool));
    // The record carries the new source-native id.
    assert!(diff.changes.iter().all(|c| c.tool_id == "one_v2"));
    // No add/remove pair: the lineage id matched.
    assert!(!diff.is_added("one") && !diff.is_removed("one"));
    assert_eq!(
        diff.changes[0].old_hash.as_deref(),
        Some(field_hash(&json!("one_tool")).as_str())
    );
}

#[test]
fn parameter_added_required() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["properties"]["c"] = json!({ "type": "string" });
    t.input_schema["required"] = json!(["a", "c"]);
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    assert_eq!(
        paths(&diff),
        vec![
            ("input_schema.properties.c".to_owned(), ChangeKind::Added),
            ("input_schema.required".to_owned(), ChangeKind::Modified)
        ]
    );
    assert!(
        diff.changes
            .iter()
            .all(|c| c.class == ChangeClass::Parameter)
    );
    let added = &diff.changes[0];
    assert_eq!(added.old_hash, None);
    assert_eq!(
        added.new_hash.as_deref(),
        Some(field_hash(&json!({ "type": "string" })).as_str())
    );
}

#[test]
fn parameter_removed() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("b");
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(c.field_path.as_deref(), Some("input_schema.properties.b"));
    assert_eq!(
        (c.class, c.kind),
        (ChangeClass::Parameter, ChangeKind::Removed)
    );
    assert!(c.old_hash.is_some() && c.new_hash.is_none());
}

#[test]
fn parameter_type_changed() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["properties"]["b"] = json!({ "type": "string" });
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(c.field_path.as_deref(), Some("input_schema.properties.b"));
    assert_eq!(
        (c.class, c.kind),
        (ChangeClass::Parameter, ChangeKind::Modified)
    );
    assert_ne!(c.old_hash, c.new_hash);
}

#[test]
fn required_changed_without_property_change() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["required"] = json!(["a", "b"]);
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(c.field_path.as_deref(), Some("input_schema.required"));
    assert_eq!(c.class, ChangeClass::Parameter);
}

#[test]
fn required_reordering_is_not_a_change() {
    let mut s = base_card("one");
    s.input_schema["required"] = json!(["a", "b"]);
    let mut t = s.clone();
    t.input_schema["required"] = json!(["b", "a"]);
    assert!(diff_kbs(&kb("v1", vec![s]), &kb("v2", vec![t])).is_empty());
}

#[test]
fn other_schema_keyword_change() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["additionalProperties"] = json!(false);
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(
        c.field_path.as_deref(),
        Some("input_schema.additionalProperties")
    );
    assert_eq!(c.kind, ChangeKind::Added);
}

#[test]
fn property_names_with_dots_are_escaped() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.input_schema["properties"]["x.y"] = json!({ "type": "string" });
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    assert_eq!(
        only(&diff).field_path.as_deref(),
        Some("input_schema.properties.x~1y")
    );
}

#[test]
fn description_changed() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.description = "Does a different thing.".into();
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(c.field_path.as_deref(), Some("description"));
    assert_eq!(
        (c.class, c.kind),
        (ChangeClass::Description, ChangeKind::Modified)
    );
}

#[test]
fn usage_rules_changed() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.usage_rules.push("Never call twice.".into());
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let c = only(&diff);
    assert_eq!(c.field_path.as_deref(), Some("usage_rules"));
    assert_eq!(c.class, ChangeClass::Description);
}

#[test]
fn output_schema_added_modified_removed() {
    let mut none = base_card("one");
    none.output_schema = None;
    let with = base_card("one");
    let mut other = base_card("one");
    other.output_schema = Some(json!({ "type": "array" }));

    let added = diff_kbs(&kb("v1", vec![none.clone()]), &kb("v2", vec![with.clone()]));
    assert_eq!(
        (
            only(&added).class,
            only(&added).kind,
            only(&added).field_path.clone()
        ),
        (
            ChangeClass::Parameter,
            ChangeKind::Added,
            Some("output_schema".to_owned())
        )
    );
    let modified = diff_kbs(&kb("v1", vec![with.clone()]), &kb("v2", vec![other]));
    assert_eq!(only(&modified).kind, ChangeKind::Modified);
    let removed = diff_kbs(&kb("v1", vec![with]), &kb("v2", vec![none]));
    assert_eq!(only(&removed).kind, ChangeKind::Removed);
    assert!(only(&removed).new_hash.is_none());
}

#[test]
fn order_is_deterministic_and_independent_of_card_order() {
    let a1 = kb("v1", vec![base_card("b"), base_card("a"), base_card("c")]);
    let mut a2 = a1.clone();
    a2.cards.reverse();

    let mut t_b = base_card("b");
    t_b.description = "changed".into();
    let mut t_a = base_card("a");
    t_a.input_schema["properties"]["z"] = json!({ "type": "string" });
    let b1 = kb("v2", vec![t_b.clone(), t_a.clone(), base_card("d")]);
    let b2 = kb("v2", vec![base_card("d"), t_a, t_b]);

    let d1 = diff_kbs(&a1, &b1);
    let d2 = diff_kbs(&a2, &b2);
    assert_eq!(d1, d2);
    let lineages: Vec<&str> = d1.changes.iter().map(|c| c.lineage_id.as_str()).collect();
    assert_eq!(lineages, ["a", "b", "c", "d"]);
    assert_eq!(
        d1.affected_lineage_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn unaffected_cards_produce_no_records() {
    let a = kb("v1", vec![base_card("keep"), base_card("edit")]);
    let mut t = base_card("edit");
    t.description = "new".into();
    let diff = diff_kbs(&a, &kb("v2", vec![base_card("keep"), t]));
    assert!(!diff.affected_lineage_ids.contains("keep"));
    assert!(diff.changes.iter().all(|c| c.lineage_id == "edit"));
}

#[test]
fn diff_serializes_to_event_shape() {
    let a = kb("v1", vec![base_card("one")]);
    let mut t = base_card("one");
    t.description = "new".into();
    let diff = diff_kbs(&a, &kb("v2", vec![t]));
    let v: Value = serde_json::to_value(&diff.changes).unwrap();
    assert_eq!(v[0]["class"], "description");
    assert_eq!(v[0]["kind"], "modified");
    assert_eq!(v[0]["lineage_id"], "one");
}
