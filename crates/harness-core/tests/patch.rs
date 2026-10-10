//! Structured patch parsing, validation against a diff, and application.

use harness_core::diff::{KbDiff, diff_kbs};
use harness_core::patch::{
    Patch, PatchError, PatchOp, apply_patch, parse_patch, patch_json_schema, validate_patch,
};
use harness_core::{Provenance, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb};
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

fn kb(version: &str, cards: Vec<ToolCard>) -> ToolKb {
    ToolKb {
        schema_version: TOOL_KB_SCHEMA_VERSION.into(),
        kb_id: format!("kb-{version}"),
        release_id: "rel".into(),
        server_version: version.into(),
        cards,
    }
}

fn set(lineage: &str, path: &str, value: Value) -> PatchOp {
    PatchOp::SetField {
        lineage_id: lineage.into(),
        field_path: path.into(),
        value,
    }
}

/// Source: keep, edit, gone. Target: keep, edit (description + new param c), new.
fn scenario() -> (ToolKb, ToolKb, KbDiff) {
    let source = kb("v1", vec![card("keep"), card("edit"), card("gone")]);
    let mut edit = card("edit");
    edit.description = "Edited.".into();
    edit.input_schema["properties"]["c"] = json!({ "type": "string" });
    edit.input_schema["required"] = json!(["a", "c"]);
    let target = kb("v2", vec![card("keep"), edit, card("new")]);
    let diff = diff_kbs(&source, &target);
    (source, target, diff)
}

fn rejects(patch: Patch, diff: &KbDiff) -> PatchError {
    validate_patch(&patch, diff).expect_err("patch must be rejected")
}

#[test]
fn schema_is_a_valid_json_schema_document() {
    let schema = patch_json_schema();
    assert_eq!(schema["required"], json!(["ops"]));
    assert!(jsonschema::validator_for(&schema).is_ok());
}

#[test]
fn parse_accepts_each_operation() {
    let new_card = serde_json::to_value(card("new")).unwrap();
    let text = json!({ "ops": [
        { "op": "set_field", "lineage_id": "edit", "field_path": "description", "value": "x" },
        { "op": "add_card", "card": new_card },
        { "op": "remove_card", "lineage_id": "gone" },
        { "op": "remove_field", "lineage_id": "edit", "field_path": "input_schema.properties.b" }
    ]})
    .to_string();
    let patch = parse_patch(&text).unwrap();
    assert_eq!(patch.ops.len(), 4);
    assert!(matches!(patch.ops[1], PatchOp::AddCard { .. }));
}

#[test]
fn parse_rejects_bad_output_without_repair() {
    let cases = [
        ("", "not JSON"),
        ("here is your patch: {\"ops\": []}", "prose around JSON"),
        ("```json\n{\"ops\": []}\n```", "code fence"),
        ("{\"ops\": [], \"extra\": 1}", "unknown top-level field"),
        ("{}", "missing ops"),
        ("{\"ops\": [{\"op\": \"delete_everything\"}]}", "unknown op"),
        (
            "{\"ops\": [{\"op\": \"set_field\", \"lineage_id\": \"a\", \"value\": 1}]}",
            "missing field_path",
        ),
        (
            "{\"ops\": [{\"op\": \"set_field\", \"lineage_id\": \"a\", \"field_path\": \"description\", \"value\": 1, \"note\": \"x\"}]}",
            "unknown op field",
        ),
        (
            "{\"ops\": [{\"op\": \"remove_card\", \"lineage_id\": \"\"}]}",
            "empty lineage id",
        ),
        (
            "{\"ops\": [{\"op\": \"add_card\", \"card\": {\"lineage_id\": \"x\"}}]}",
            "incomplete card",
        ),
    ];
    for (text, why) in cases {
        let err = parse_patch(text).expect_err(why);
        assert!(matches!(err, PatchError::Malformed(_)), "{why}: {err:?}");
    }
}

#[test]
fn serde_rejects_unknown_fields_too() {
    let bad = r#"{"ops":[{"op":"remove_card","lineage_id":"a","why":"x"}]}"#;
    assert!(serde_json::from_str::<Patch>(bad).is_err());
}

#[test]
fn validate_accepts_a_patch_that_matches_the_diff() {
    let (_, _, diff) = scenario();
    let patch = Patch {
        ops: vec![
            set("edit", "description", json!("Edited.")),
            set(
                "edit",
                "input_schema.properties.c",
                json!({ "type": "string" }),
            ),
            set("edit", "input_schema.required", json!(["a", "c"])),
            PatchOp::AddCard {
                card: Box::new(card("new")),
            },
            PatchOp::RemoveCard {
                lineage_id: "gone".into(),
            },
        ],
    };
    assert_eq!(validate_patch(&patch, &diff), Ok(()));
    // Narrower than a detected change is fine.
    let narrower = Patch {
        ops: vec![set(
            "edit",
            "input_schema.properties.c.type",
            json!("string"),
        )],
    };
    assert_eq!(validate_patch(&narrower, &diff), Ok(()));
    assert_eq!(validate_patch(&Patch::default(), &diff), Ok(()));
}

#[test]
fn validate_rejects_unaffected_lineage() {
    let (_, _, diff) = scenario();
    let err = rejects(
        Patch {
            ops: vec![set("keep", "description", json!("x"))],
        },
        &diff,
    );
    assert!(
        matches!(err, PatchError::OutsideDiff { index: 0, .. }),
        "{err:?}"
    );
    let err = rejects(
        Patch {
            ops: vec![set("nonexistent", "description", json!("x"))],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
}

#[test]
fn validate_rejects_field_outside_detected_change() {
    let (_, _, diff) = scenario();
    // usage_rules did not change for "edit".
    let err = rejects(
        Patch {
            ops: vec![set("edit", "usage_rules", json!([]))],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
    // A parameter that did not change.
    let err = rejects(
        Patch {
            ops: vec![set(
                "edit",
                "input_schema.properties.a",
                json!({ "type": "integer" }),
            )],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
    // Renaming when only the description changed.
    let err = rejects(
        Patch {
            ops: vec![set("edit", "name", json!("other"))],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
}

#[test]
fn validate_never_broadens_to_an_ancestor_path() {
    let (_, _, diff) = scenario();
    for path in ["input_schema", "input_schema.properties"] {
        let err = rejects(
            Patch {
                ops: vec![set("edit", path, json!({ "type": "object" }))],
            },
            &diff,
        );
        assert!(
            matches!(err, PatchError::OutsideDiff { .. }),
            "{path}: {err:?}"
        );
    }
}

#[test]
fn validate_rejects_forbidden_and_malformed_paths() {
    let (_, _, diff) = scenario();
    for path in [
        "lineage_id",
        "server_id",
        "server_version",
        "provenance",
        "provenance.source_file",
    ] {
        let err = rejects(
            Patch {
                ops: vec![set("edit", path, json!("x"))],
            },
            &diff,
        );
        assert!(
            matches!(err, PatchError::ForbiddenField { .. }),
            "{path}: {err:?}"
        );
    }
    for path in ["", "description.", ".description", "input_schema..x"] {
        let err = rejects(
            Patch {
                ops: vec![set("edit", path, json!("x"))],
            },
            &diff,
        );
        assert!(
            matches!(err, PatchError::InvalidPath { .. }),
            "{path:?}: {err:?}"
        );
    }
}

#[test]
fn validate_rejects_field_edits_on_added_or_removed_cards() {
    let (_, _, diff) = scenario();
    for lineage in ["new", "gone"] {
        let err = rejects(
            Patch {
                ops: vec![set(lineage, "description", json!("x"))],
            },
            &diff,
        );
        assert!(
            matches!(err, PatchError::OutsideDiff { .. }),
            "{lineage}: {err:?}"
        );
    }
}

#[test]
fn validate_rejects_add_and_remove_outside_the_diff() {
    let (_, _, diff) = scenario();
    let err = rejects(
        Patch {
            ops: vec![PatchOp::AddCard {
                card: Box::new(card("surprise")),
            }],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
    // Adding a card that already exists in the diff as modified, not added.
    let err = rejects(
        Patch {
            ops: vec![PatchOp::AddCard {
                card: Box::new(card("edit")),
            }],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
    // Removing a tool that still exists.
    let err = rejects(
        Patch {
            ops: vec![PatchOp::RemoveCard {
                lineage_id: "keep".into(),
            }],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
    let err = rejects(
        Patch {
            ops: vec![PatchOp::RemoveCard {
                lineage_id: "edit".into(),
            }],
        },
        &diff,
    );
    assert!(matches!(err, PatchError::OutsideDiff { .. }));
}

#[test]
fn validate_rejects_duplicates() {
    let (_, _, diff) = scenario();
    let dup_field = Patch {
        ops: vec![
            set("edit", "description", json!("a")),
            set("edit", "description", json!("b")),
        ],
    };
    assert!(matches!(
        rejects(dup_field, &diff),
        PatchError::Duplicate { index: 1, .. }
    ));
    let dup_add = Patch {
        ops: vec![
            PatchOp::AddCard {
                card: Box::new(card("new")),
            },
            PatchOp::AddCard {
                card: Box::new(card("new")),
            },
        ],
    };
    assert!(matches!(
        rejects(dup_add, &diff),
        PatchError::Duplicate { index: 1, .. }
    ));
}

#[test]
fn remove_field_is_limited_to_nested_schema_keys() {
    let source = kb("v1", vec![card("edit")]);
    let mut t = card("edit");
    t.input_schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("b");
    let diff = diff_kbs(&source, &kb("v2", vec![t]));

    let ok = Patch {
        ops: vec![PatchOp::RemoveField {
            lineage_id: "edit".into(),
            field_path: "input_schema.properties.b".into(),
        }],
    };
    assert_eq!(validate_patch(&ok, &diff), Ok(()));

    let top = Patch {
        ops: vec![PatchOp::RemoveField {
            lineage_id: "edit".into(),
            field_path: "description".into(),
        }],
    };
    assert!(matches!(
        rejects(top, &diff),
        PatchError::ForbiddenField { .. }
    ));
    let other = Patch {
        ops: vec![PatchOp::RemoveField {
            lineage_id: "edit".into(),
            field_path: "input_schema.properties.a".into(),
        }],
    };
    assert!(matches!(
        rejects(other, &diff),
        PatchError::OutsideDiff { .. }
    ));
}

#[test]
fn apply_sets_fields_and_leaves_input_untouched() {
    let (source, _, _) = scenario();
    let before = source.clone();
    let patch = Patch {
        ops: vec![
            set("edit", "description", json!("Edited.")),
            set(
                "edit",
                "input_schema.properties.c",
                json!({ "type": "string" }),
            ),
            set("edit", "input_schema.required", json!(["a", "c"])),
            set("edit", "tool_id", json!("edit_v2")),
            set("edit", "usage_rules", json!(["r1", "r2"])),
            set("edit", "output_schema", Value::Null),
        ],
    };
    let out = apply_patch(&source, &patch).unwrap();
    assert_eq!(source, before, "input KB must not be modified");
    let edit = out.cards.iter().find(|c| c.lineage_id == "edit").unwrap();
    assert_eq!(edit.description, "Edited.");
    assert_eq!(edit.tool_id, "edit_v2");
    assert_eq!(
        edit.input_schema["properties"]["c"],
        json!({ "type": "string" })
    );
    assert_eq!(edit.input_schema["required"], json!(["a", "c"]));
    assert_eq!(edit.usage_rules, ["r1", "r2"]);
    assert_eq!(edit.output_schema, None);
    // Other cards untouched, order preserved.
    let ids: Vec<_> = out.cards.iter().map(|c| c.lineage_id.as_str()).collect();
    assert_eq!(ids, ["keep", "edit", "gone"]);
    assert_eq!(out.cards[0], source.cards[0]);
    assert_eq!(out.cards[2], source.cards[2]);
}

#[test]
fn apply_nested_output_schema_and_remove_field() {
    let source = kb("v1", vec![card("edit")]);
    let patch = Patch {
        ops: vec![
            set(
                "edit",
                "output_schema.properties.extra",
                json!({ "type": "integer" }),
            ),
            PatchOp::RemoveField {
                lineage_id: "edit".into(),
                field_path: "input_schema.properties.b".into(),
            },
        ],
    };
    let out = apply_patch(&source, &patch).unwrap();
    let c = &out.cards[0];
    assert_eq!(
        c.output_schema.as_ref().unwrap()["properties"]["extra"],
        json!({ "type": "integer" })
    );
    assert!(c.input_schema["properties"].get("b").is_none());
    assert!(c.input_schema["properties"].get("a").is_some());
}

#[test]
fn apply_escaped_segments_address_dotted_names() {
    let mut c = card("edit");
    c.input_schema["properties"]["x.y"] = json!({ "type": "string" });
    let source = kb("v1", vec![c]);
    let patch = Patch {
        ops: vec![set(
            "edit",
            "input_schema.properties.x~1y.type",
            json!("integer"),
        )],
    };
    let out = apply_patch(&source, &patch).unwrap();
    assert_eq!(
        out.cards[0].input_schema["properties"]["x.y"]["type"],
        "integer"
    );
}

#[test]
fn apply_adds_and_removes_cards() {
    let (source, _, _) = scenario();
    let patch = Patch {
        ops: vec![
            PatchOp::AddCard {
                card: Box::new(card("new")),
            },
            PatchOp::RemoveCard {
                lineage_id: "gone".into(),
            },
        ],
    };
    let out = apply_patch(&source, &patch).unwrap();
    let ids: Vec<_> = out.cards.iter().map(|c| c.lineage_id.as_str()).collect();
    assert_eq!(ids, ["keep", "edit", "new"]);
}

#[test]
fn apply_reports_structural_errors() {
    let (source, _, _) = scenario();
    let unknown = apply_patch(
        &source,
        &Patch {
            ops: vec![set("ghost", "description", json!("x"))],
        },
    );
    assert!(matches!(unknown, Err(PatchError::UnknownCard { .. })));

    let exists = apply_patch(
        &source,
        &Patch {
            ops: vec![PatchOp::AddCard {
                card: Box::new(card("keep")),
            }],
        },
    );
    assert!(matches!(exists, Err(PatchError::CardExists { .. })));

    let remove_missing = apply_patch(
        &source,
        &Patch {
            ops: vec![PatchOp::RemoveCard {
                lineage_id: "ghost".into(),
            }],
        },
    );
    assert!(matches!(
        remove_missing,
        Err(PatchError::UnknownCard { .. })
    ));

    let bad_type = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "description", json!(5))],
        },
    );
    assert!(matches!(bad_type, Err(PatchError::InvalidValue { .. })));

    let bad_rules = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "usage_rules", json!([1]))],
        },
    );
    assert!(matches!(bad_rules, Err(PatchError::InvalidValue { .. })));

    let empty_id = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "tool_id", json!(""))],
        },
    );
    assert!(matches!(empty_id, Err(PatchError::InvalidValue { .. })));

    let bad_schema = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "input_schema", json!("x"))],
        },
    );
    assert!(matches!(bad_schema, Err(PatchError::InvalidValue { .. })));

    // No auto-vivification of missing containers.
    let missing_parent = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "input_schema.nope.deeper", json!(1))],
        },
    );
    assert!(matches!(
        missing_parent,
        Err(PatchError::InvalidPath { .. })
    ));

    let nested_on_string = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "description.more", json!("x"))],
        },
    );
    assert!(matches!(
        nested_on_string,
        Err(PatchError::InvalidPath { .. })
    ));

    let remove_missing_key = apply_patch(
        &source,
        &Patch {
            ops: vec![PatchOp::RemoveField {
                lineage_id: "edit".into(),
                field_path: "input_schema.properties.zzz".into(),
            }],
        },
    );
    assert!(matches!(
        remove_missing_key,
        Err(PatchError::InvalidPath { .. })
    ));
}

#[test]
fn apply_ambiguous_lineage_is_an_error() {
    let mut second = card("edit");
    second.server_id = "other".into();
    let source = kb("v1", vec![card("edit"), second]);
    let out = apply_patch(
        &source,
        &Patch {
            ops: vec![set("edit", "description", json!("x"))],
        },
    );
    assert!(matches!(out, Err(PatchError::AmbiguousCard { .. })));
}

#[test]
fn applying_the_diff_derived_patch_reaches_the_target_structure() {
    let (source, target, diff) = scenario();
    let patch = Patch {
        ops: vec![
            set("edit", "description", json!("Edited.")),
            set(
                "edit",
                "input_schema.properties.c",
                json!({ "type": "string" }),
            ),
            set("edit", "input_schema.required", json!(["a", "c"])),
            PatchOp::AddCard {
                card: Box::new(card("new")),
            },
            PatchOp::RemoveCard {
                lineage_id: "gone".into(),
            },
        ],
    };
    validate_patch(&patch, &diff).unwrap();
    let out = apply_patch(&source, &patch).unwrap();
    // After patching, the diff towards the target no longer lists content changes for `edit`.
    let residual = diff_kbs(&out, &target);
    assert!(residual.is_empty(), "{:#?}", residual.changes);
}
