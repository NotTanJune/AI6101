//! MCPEvol-Bench adapter over the invented miniature in `fixtures/mcpevol-mini`. No network and
//! no reliance on the real benchmark clone.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::diff::{KbDiff, diff_kbs};
use harness_core::mcpevol::{
    BRIEF_EXPECTED_SERVERS, BRIEF_EXPECTED_TASKS, BRIEF_EXPECTED_TOOLS, McpEvolAdapter,
    McpEvolOptions, StageLabels, VERSION_LABELS, evolution_file,
};
use harness_core::pipeline::{EnvLookup, RunOptions, run_matrix, validate_config_inputs};
use harness_core::source::SourceAdapter;
use harness_core::{
    ChangeClass, ChangeKind, Config, ConfigError, FixedClock, HarnessError, RunStatus, ToolKb,
    VersionStage,
};
use sha2::{Digest, Sha256};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn mini_data() -> PathBuf {
    repo_root().join("fixtures/mcpevol-mini/data")
}

fn open(options: McpEvolOptions) -> McpEvolAdapter {
    let d = mini_data();
    McpEvolAdapter::open(&d, &d, options).expect("mini fixture opens")
}

fn open_default() -> McpEvolAdapter {
    open(McpEvolOptions::default())
}

fn env_for(dir: &Path) -> EnvLookup {
    let dir = dir.to_string_lossy().into_owned();
    Arc::new(move |name| match name {
        "MCP_DATA_DIR" | "EVAL_DATA_DIR" => Some(dir.clone()),
        _ => None,
    })
}

fn mini_config() -> Config {
    Config::load(repo_root().join("fixtures/mcpevol-mini.toml")).expect("mini config loads")
}

fn changes(diff: &KbDiff) -> BTreeSet<(String, ChangeClass, ChangeKind, Option<String>)> {
    diff.changes
        .iter()
        .map(|c| (c.lineage_id.clone(), c.class, c.kind, c.field_path.clone()))
        .collect()
}

fn expect(
    rows: &[(&str, ChangeClass, ChangeKind, Option<&str>)],
) -> BTreeSet<(String, ChangeClass, ChangeKind, Option<String>)> {
    rows.iter()
        .map(|(l, c, k, f)| ((*l).to_owned(), *c, *k, f.map(str::to_owned)))
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

/// A writable copy of the mini data directory.
fn scratch_copy() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    copy_dir(&mini_data(), &data);
    (tmp, data)
}

fn edit_json(path: &Path, f: impl FnOnce(&mut serde_json::Value)) {
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    f(&mut v);
    fs::write(path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

fn card<'a>(kb: &'a ToolKb, lineage: &str) -> &'a harness_core::ToolCard {
    kb.cards
        .iter()
        .find(|c| c.lineage_id == lineage)
        .unwrap_or_else(|| panic!("no card {lineage} in {}", kb.server_version))
}

#[test]
fn version_labels_are_original_then_iter1_to_iter4() {
    let a = open_default();
    assert_eq!(
        a.version_labels(),
        vec!["original", "iter1", "iter2", "iter3", "iter4"]
    );
    assert_eq!(VERSION_LABELS.len(), 5);
    for label in a.version_labels() {
        let kb = a.load_kb(&label).unwrap();
        assert_eq!(kb.server_version, label);
        assert_eq!(kb.schema_version, "tool-kb/1");
        assert!(kb.release_id.starts_with("mcpevol-"));
        // Every KB passes the tool-kb/1 schema.
        ToolKb::from_json_str(&serde_json::to_string(&kb).unwrap()).expect("schema-valid KB");
        for c in &kb.cards {
            assert_eq!(c.server_version, label);
            assert!(c.usage_rules.is_empty());
        }
    }
    let err = a.load_kb("iter5").unwrap_err().to_string();
    assert!(err.contains("unknown version"), "{err}");
    assert!(err.contains("iter4"), "{err}");
}

#[test]
fn default_stage_mapping_is_the_project_decision_and_configurable() {
    let a = open_default();
    assert_eq!(a.stage_depths(), [0, 2, 4]);
    let inv = a.inventory().unwrap();
    for server in &inv.servers {
        let stages: Vec<(String, Option<VersionStage>)> = server
            .versions
            .iter()
            .map(|v| (v.label.clone(), v.stage))
            .collect();
        assert_eq!(
            stages,
            vec![
                ("original".to_owned(), Some(VersionStage::Early)),
                ("iter1".to_owned(), None),
                ("iter2".to_owned(), Some(VersionStage::Middle)),
                ("iter3".to_owned(), None),
                ("iter4".to_owned(), Some(VersionStage::Late)),
            ]
        );
    }
    let custom = open(McpEvolOptions {
        stages: StageLabels {
            early: "original".into(),
            middle: "iter1".into(),
            late: "iter3".into(),
        },
        ..McpEvolOptions::default()
    });
    assert_eq!(custom.stage_depths(), [0, 1, 3]);

    let bad = |early: &str, middle: &str, late: &str| {
        let d = mini_data();
        McpEvolAdapter::open(
            &d,
            &d,
            McpEvolOptions {
                stages: StageLabels {
                    early: early.into(),
                    middle: middle.into(),
                    late: late.into(),
                },
                ..McpEvolOptions::default()
            },
        )
        .unwrap_err()
        .to_string()
    };
    assert!(bad("original", "iter9", "iter4").contains("unknown version"));
    assert!(bad("iter3", "iter2", "iter4").contains("must not go backwards"));
}

#[test]
fn replay_is_cumulative_oldest_first() {
    let a = open_default();
    let prop = |kb: &ToolKb, lineage: &str, name: &str| {
        card(kb, lineage).input_schema["properties"]
            .get(name)
            .cloned()
    };
    let kbs: Vec<ToolKb> = VERSION_LABELS
        .iter()
        .map(|l| a.load_kb(l).unwrap())
        .collect();

    // priority: added at depth 1 and stays.
    assert!(prop(&kbs[0], "alpha-notes::create_note", "priority").is_none());
    for kb in &kbs[1..] {
        assert!(prop(kb, "alpha-notes::create_note", "priority").is_some());
    }
    // tags: depth 3 only.
    assert!(prop(&kbs[2], "alpha-notes::create_note", "tags").is_none());
    assert!(prop(&kbs[3], "alpha-notes::create_note", "tags").is_some());
    // sort: present until depth 4 removes it.
    assert!(prop(&kbs[3], "alpha-notes::list_notes", "sort").is_some());
    assert!(prop(&kbs[4], "alpha-notes::list_notes", "sort").is_none());
    // list_notes description: changed at depth 2 (visible at iter2, not at iter1).
    assert_eq!(
        card(&kbs[1], "alpha-notes::list_notes").description,
        "List notes."
    );
    assert_eq!(
        card(&kbs[2], "alpha-notes::list_notes").description,
        "List notes, newest first."
    );
    // Tool counts only grow or shrink at the depth the mutation sits in.
    let counts: Vec<usize> = kbs.iter().map(|k| k.cards.len()).collect();
    assert_eq!(counts, vec![7, 7, 8, 9, 9]);

    // The cumulative file order is the nested layout, shallowest first.
    assert_eq!(evolution_file(1), "mutation/HYBRID/evolution.json");
    assert_eq!(evolution_file(2), "mutation/HYBRID/HYBRID/evolution.json");
}

#[test]
fn every_mutation_shape_maps_to_the_expected_diff() {
    use ChangeClass::{Description, Parameter, Tool};
    use ChangeKind::{Added, Modified, Removed};
    let a = open_default();
    let kb = |l: &str| a.load_kb(l).unwrap();
    let step = |from: &str, to: &str| changes(&diff_kbs(&kb(from), &kb(to)));

    // depth 1: PARAM additions (loose enum type, snapshot definition kept exactly), DESC with a
    // null original (tool + parameters), TOOL delete already applied in the snapshot (no diff).
    assert_eq!(
        step("original", "iter1"),
        expect(&[
            (
                "alpha-notes::create_note",
                Parameter,
                Added,
                Some("input_schema.properties.priority")
            ),
            (
                "gamma-docs::get_doc",
                Parameter,
                Modified,
                Some("input_schema.properties.path")
            ),
            (
                "gamma-docs::get_doc",
                Description,
                Modified,
                Some("description")
            ),
        ])
    );
    // depth 2: DESC tool + parameters; TOOL add via new_tool_config.
    assert_eq!(
        step("iter1", "iter2"),
        expect(&[
            ("@beta/calendar::find_event", Tool, Added, None),
            (
                "alpha-notes::list_notes",
                Parameter,
                Modified,
                Some("input_schema.properties.limit")
            ),
            (
                "alpha-notes::list_notes",
                Description,
                Modified,
                Some("description")
            ),
        ])
    );
    // depth 3: PARAM changes (add, remove, required, tool description), flat DESC, TOOL add via
    // auto_optimizations.
    assert_eq!(
        step("iter2", "iter3"),
        expect(&[
            ("@beta/calendar::bulk_add_events", Tool, Added, None),
            (
                "alpha-notes::create_note",
                Parameter,
                Removed,
                Some("input_schema.properties.legacy_flag")
            ),
            (
                "alpha-notes::create_note",
                Parameter,
                Added,
                Some("input_schema.properties.tags")
            ),
            (
                "alpha-notes::create_note",
                Parameter,
                Modified,
                Some("input_schema.required")
            ),
            (
                "alpha-notes::create_note",
                Description,
                Modified,
                Some("description")
            ),
            (
                "gamma-docs::get_other",
                Description,
                Modified,
                Some("description")
            ),
        ])
    );
    // depth 4: PARAM removals, TOOL delete with migration, TOOL add from a full config.
    assert_eq!(
        step("iter3", "iter4"),
        expect(&[
            ("@beta/calendar::remove_event", Tool, Removed, None),
            (
                "alpha-notes::list_notes",
                Parameter,
                Removed,
                Some("input_schema.properties.offset")
            ),
            (
                "alpha-notes::list_notes",
                Parameter,
                Removed,
                Some("input_schema.properties.sort")
            ),
            ("gamma-docs::get_summary", Tool, Added, None),
        ])
    );
}

#[test]
fn replayed_cards_carry_traceable_provenance() {
    let a = open_default();
    let kb = a.load_kb("iter4").unwrap();
    let create = card(&kb, "alpha-notes::create_note");
    assert_eq!(create.provenance.release_id, a.release_id());
    assert_eq!(create.provenance.source_file, "test_cases/status_dict.json");
    let pointer = create.provenance.source_pointer.clone().unwrap();
    assert!(
        pointer.starts_with("status_dict:/alpha-notes/tools/0"),
        "{pointer}"
    );
    assert!(pointer.contains("evolution:d1:alpha-notes/create_note#parameter_additions"));
    assert!(pointer.contains("evolution:d3:alpha-notes/create_note#parameter_changes"));
    assert!(pointer.contains("evolution:d3:alpha-notes/create_note#tool_description_change"));
    // The original card says it was rewound.
    let original = a.load_kb("original").unwrap();
    let rewound = card(&original, "alpha-notes::create_note");
    assert!(
        rewound
            .provenance
            .source_pointer
            .as_deref()
            .unwrap()
            .contains("rewind:d1:alpha-notes/create_note#parameter_additions")
    );
    // A card built from new_tool_config has no snapshot pointer and names the evolution file.
    let summary = card(&kb, "gamma-docs::get_summary");
    assert_eq!(summary.provenance.source_file, evolution_file(4));
    assert_eq!(
        summary.provenance.source_pointer.as_deref(),
        Some("evolution:d4:gamma-docs/get_summary#new_tool_config")
    );
    assert!(summary.output_schema.is_some());
    // JSON-pointer escaping of '/' in server names.
    let find = card(&kb, "@beta/calendar::find_event");
    assert!(
        find.provenance
            .source_pointer
            .as_deref()
            .unwrap()
            .starts_with("status_dict:/@beta~1calendar/tools/3")
    );
    // Every card in every version has a pointer except untouched ones, which still have the
    // snapshot location.
    for label in VERSION_LABELS {
        for c in &a.load_kb(label).unwrap().cards {
            assert!(
                c.provenance.source_pointer.is_some(),
                "{label} {}",
                c.lineage_id
            );
        }
    }
}

#[test]
fn lineage_is_server_and_original_tool_name_and_deletes_are_not_renames() {
    let a = open_default();
    let kb = a.load_kb("iter4").unwrap();
    for c in &kb.cards {
        assert_eq!(c.lineage_id, format!("{}::{}", c.server_id, c.tool_id));
    }
    // A migration target is NOT lineage: deleting remove_event is a plain removal and no
    // Renamed change exists anywhere.
    let diff = diff_kbs(&a.load_kb("original").unwrap(), &kb);
    assert!(diff.is_removed("@beta/calendar::remove_event"));
    assert!(!diff.changes.iter().any(|c| c.kind == ChangeKind::Renamed));
    assert!(diff.is_added("@beta/calendar::find_event"));
}

#[test]
fn explicit_old_tool_name_keeps_lineage() {
    // The runner supports `old_tool_name` + `new_tool_config` as a replacement; the supplied
    // data has no instance, so the shape is exercised on a modified copy.
    let (_tmp, data) = scratch_copy();
    edit_json(&data.join(evolution_file(2)), |v| {
        v["calendar"]["old_tool_name"] = "remove_event".into();
    });
    let a = McpEvolAdapter::open(&data, &data, McpEvolOptions::default()).unwrap();
    let iter2 = a.load_kb("iter2").unwrap();
    let renamed = card(&iter2, "@beta/calendar::remove_event");
    assert_eq!(renamed.tool_id, "find_event");
    let diff = diff_kbs(&a.load_kb("original").unwrap(), &iter2);
    let kinds: BTreeSet<_> = diff
        .changes_for("@beta/calendar::remove_event")
        .map(|c| (c.kind, c.field_path.clone()))
        .collect();
    assert!(
        kinds.contains(&(ChangeKind::Renamed, Some("tool_id".to_owned()))),
        "{kinds:?}"
    );
    assert!(!diff.is_added("@beta/calendar::find_event"));
}

#[test]
fn server_names_resolve_and_unresolved_ones_are_reported() {
    let a = open_default();
    let r = a.report();
    assert_eq!(r.referenced_servers, 4);
    assert_eq!(r.resolved_servers, 3);
    assert_eq!(
        r.renamed_servers.iter().collect::<Vec<_>>(),
        vec![(&"calendar".to_owned(), &"@beta/calendar".to_owned())]
    );
    assert_eq!(r.unresolved_servers.len(), 1);
    assert_eq!(r.unresolved_servers[0].name, "ghost-server");
    assert_eq!(r.unresolved_servers[0].task_count, 1);
    // The evolution entry that names the server as "calendar" was applied to the real key.
    assert!(
        a.load_kb("iter2")
            .unwrap()
            .cards
            .iter()
            .any(|c| c.tool_id == "find_event")
    );
    // The unresolved server is not invented into the KB or inventory.
    let inv = a.inventory().unwrap();
    assert!(inv.servers.iter().all(|s| s.server_id != "ghost-server"));
    // A tool named by a task that exists in no version is reported, not dropped silently.
    assert_eq!(r.unresolved_tools.len(), 1);
    assert_eq!(r.unresolved_tools[0].server, "@beta/calendar");
    assert_eq!(r.unresolved_tools[0].tool, "legacy_event");
    // The mutation entry for the unresolved server is surfaced as unmapped.
    assert!(
        r.unmapped
            .iter()
            .any(|u| u.server == "ghost-server" && u.reason == "server_not_in_status_dict")
    );
}

#[test]
fn task_stage_and_change_classes_follow_the_mutations_that_involve_the_task() {
    use ChangeClass::{Description, Parameter, Tool};
    let a = open_default();
    let inv = a.inventory().unwrap();
    assert_eq!(inv.tasks.len(), 5, "all chosen tasks, not only the scope");
    let ids: Vec<&str> = inv.tasks.iter().map(|t| t.task_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["0", "3", "5", "7", "9"],
        "chosen_idx order, idx as decimal string"
    );
    let task = |id: &str| inv.tasks.iter().find(|t| t.task_id == id).unwrap().clone();

    // Task 0: create_note PARAM (depth 1), list_notes DESC, calendar TOOL add (depth 2).
    let t0 = task("0");
    assert_eq!(t0.server_id, "alpha-notes");
    assert_eq!(t0.server_ids, vec!["alpha-notes", "@beta/calendar"]);
    assert!(t0.prompt.starts_with("Create a high priority note"));
    assert_eq!(t0.stage, VersionStage::Middle);
    let classes: BTreeSet<_> = t0.change_classes.iter().copied().collect();
    assert_eq!(classes, BTreeSet::from([Tool, Parameter, Description]));
    // Task 3 only lists delete_note, which no mutation touches: Early, no classes.
    let t3 = task("3");
    assert_eq!(t3.stage, VersionStage::Early);
    assert!(t3.change_classes.is_empty());
    // Task 5: get_doc DESC at depth 1 becomes visible at Middle (iter2).
    assert_eq!(task("5").stage, VersionStage::Middle);
    // Task 7: first server is the resolved key; the unresolved name stays in server_ids.
    let t7 = task("7");
    assert_eq!(t7.server_id, "@beta/calendar");
    assert_eq!(t7.server_ids, vec!["@beta/calendar", "ghost-server"]);
    assert_eq!(t7.stage, VersionStage::Middle);
    // Task 9: get_other DESC at depth 3 and the gamma TOOL add at depth 4: first visible at Late.
    let t9 = task("9");
    assert_eq!(t9.stage, VersionStage::Late);
    assert_eq!(
        t9.change_classes.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([Tool, Description])
    );

    // Changing the stage mapping changes the derivation: with Late = iter3 the depth-4 TOOL
    // add is beyond the late stage and drops out.
    let custom = open(McpEvolOptions {
        stages: StageLabels {
            early: "original".into(),
            middle: "iter1".into(),
            late: "iter3".into(),
        },
        ..McpEvolOptions::default()
    });
    let inv = custom.inventory().unwrap();
    let t9 = inv.tasks.iter().find(|t| t.task_id == "9").unwrap();
    assert_eq!(t9.stage, VersionStage::Late);
    assert_eq!(t9.change_classes, vec![Description]);
    let t5 = inv.tasks.iter().find(|t| t.task_id == "5").unwrap();
    assert_eq!(
        t5.stage,
        VersionStage::Middle,
        "depth 1 is visible at iter1"
    );
}

#[test]
fn scope_filtering_limits_the_kb_but_not_the_task_list() {
    let a = open(McpEvolOptions {
        scope_task_ids: Some(vec!["3".into()]),
        ..McpEvolOptions::default()
    });
    let inv = a.inventory().unwrap();
    assert_eq!(inv.tasks.len(), 5);
    assert_eq!(inv.servers.len(), 1);
    assert_eq!(inv.servers[0].server_id, "alpha-notes");
    let kb = a.load_kb("iter4").unwrap();
    assert_eq!(
        kb.cards
            .iter()
            .map(|c| c.tool_id.as_str())
            .collect::<Vec<_>>(),
        vec!["delete_note"]
    );
    assert_eq!(a.report().scoped_tasks, 1);

    // Task 0 scopes two servers; tools introduced by TOOL mutations on them are included so a
    // Tool-level add is visible, but a tool no task names and no mutation introduces is not.
    let a = open(McpEvolOptions {
        scope_task_ids: Some(vec!["0".into(), "not-a-task".into()]),
        ..McpEvolOptions::default()
    });
    assert_eq!(a.report().unknown_scope_ids, vec!["not-a-task".to_owned()]);
    let names: BTreeSet<String> = a
        .load_kb("iter4")
        .unwrap()
        .cards
        .iter()
        .map(|c| c.tool_id.clone())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "create_note".to_owned(),
            "list_notes".to_owned(),
            "add_event".to_owned(),
            "find_event".to_owned(),
            "bulk_add_events".to_owned()
        ])
    );
    assert!(!names.contains("list_events"));
    assert!(!names.contains("delete_note"));
}

#[test]
fn unmapped_entries_are_surfaced_with_reasons_and_original_text() {
    let a = open_default();
    let r = a.report();
    let by_reason =
        |reason: &str| -> Vec<_> { r.unmapped.iter().filter(|u| u.reason == reason).collect() };
    let free = by_reason("free_text_change");
    assert_eq!(free.len(), 1);
    assert_eq!(free[0].depth, 3);
    assert!(
        free[0].detail.contains("string (trimmed, min 3)"),
        "{}",
        free[0].detail
    );
    let auto = by_reason("optimized_description_text_unavailable");
    assert_eq!(auto.len(), 1);
    assert_eq!(auto[0].tool.as_deref(), Some("add_event"));
    assert!(
        auto[0]
            .detail
            .contains("Added a note pointing to bulk_add_events.")
    );
    let shape = by_reason("unrecognized_shape");
    assert_eq!(shape.len(), 1);
    assert_eq!(shape[0].server, "gamma-docs");
    assert_eq!(by_reason("server_not_in_status_dict").len(), 1);
    assert_eq!(r.unmapped.len(), 4, "{:#?}", r.unmapped);

    // Notes: baked-in structural changes were rewound, and removed parameters or deleted tools
    // that cannot be restored faithfully say so.
    let kinds: BTreeSet<&str> = r.notes.iter().map(|n| n.kind).collect();
    for needed in [
        "reconstructed",
        "not_reconstructable",
        "from_snapshot",
        "already_applied",
    ] {
        assert!(
            kinds.contains(needed),
            "missing note kind {needed}: {kinds:?}"
        );
    }
}

#[test]
fn a_new_tool_without_a_definition_is_unmapped_not_invented() {
    let (_tmp, data) = scratch_copy();
    edit_json(&data.join("test_cases/status_dict.json"), |v| {
        let tools = v["@beta/calendar"]["tools"].as_array_mut().unwrap();
        tools.retain(|t| t["name"] != "bulk_add_events");
    });
    let a = McpEvolAdapter::open(&data, &data, McpEvolOptions::default()).unwrap();
    assert!(
        a.report()
            .unmapped
            .iter()
            .any(|u| u.reason == "tool_definition_unavailable"
                && u.tool.as_deref() == Some("bulk_add_events"))
    );
    assert!(
        a.load_kb("iter4")
            .unwrap()
            .cards
            .iter()
            .all(|c| c.tool_id != "bulk_add_events")
    );
}

#[test]
fn disabling_the_rewind_replays_onto_the_snapshot_as_is() {
    let a = open(McpEvolOptions {
        rewind_baseline: false,
        ..McpEvolOptions::default()
    });
    let original = a.load_kb("original").unwrap();
    // The snapshot already contains the depth-1 priority parameter and the depth-2 tool.
    assert!(
        card(&original, "alpha-notes::create_note").input_schema["properties"]
            .get("priority")
            .is_some()
    );
    assert!(original.cards.iter().any(|c| c.tool_id == "find_event"));
    assert!(
        a.describe()
            .iter()
            .any(|l| l.contains("rewind is disabled")),
        "validate output must say the rewind is off"
    );
}

#[test]
fn commit_is_discovered_only_for_a_benchmark_shaped_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("clone");
    let data = root.join("src/custom/data");
    copy_dir(&mini_data(), &data);
    let sha = "0123456789abcdef0123456789abcdef01234567";
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(root.join(".git/refs/heads/main"), format!("{sha}\n")).unwrap();
    let a = McpEvolAdapter::open(&data, &data, McpEvolOptions::default()).unwrap();
    assert_eq!(a.release_id(), format!("mcpevol-{sha}"));

    // Not benchmark-shaped (the data dir is not <root>/src/custom/data): fall back to a short
    // content hash, never the enclosing repository's commit.
    let odd = tmp.path().join("clone/elsewhere");
    copy_dir(&mini_data(), &odd);
    let b = McpEvolAdapter::open(&odd, &odd, McpEvolOptions::default()).unwrap();
    assert!(b.release_id().starts_with("mcpevol-"));
    assert_eq!(b.release_id().len(), "mcpevol-".len() + 12);
    // The mini fixture itself lives inside this project's repository and still gets the hash.
    assert_eq!(open_default().release_id().len(), "mcpevol-".len() + 12);
}

#[test]
fn inputs_are_never_modified() {
    let before: Vec<(String, String)> = {
        let a = open_default();
        a.report()
            .files
            .iter()
            .map(|f| (f.path.clone(), f.sha256.clone()))
            .collect()
    };
    let _ = open_default().load_kb("iter4").unwrap();
    let after: Vec<(String, String)> = open_default()
        .report()
        .files
        .iter()
        .map(|f| (f.path.clone(), f.sha256.clone()))
        .collect();
    assert_eq!(before, after);
    assert_eq!(before.len(), 7);
    for (path, sha) in &before {
        let bytes = fs::read(mini_data().join(path)).unwrap();
        assert_eq!(&hex::encode(Sha256::digest(&bytes)), sha, "{path}");
        assert!(!path.starts_with('/'), "source-relative names only: {path}");
    }
}

// ---------------------------------------------------------------------------------------------
// Config, validate output, manifest and run
// ---------------------------------------------------------------------------------------------

#[test]
fn validate_prints_hashes_counts_stage_mapping_and_the_brief_comparison() {
    let report = validate_config_inputs(&mini_config(), &*env_for(&mini_data())).expect("valid");
    let text = report.to_string();
    for needle in [
        "adapter mcpevol",
        "test_cases/status_dict.json",
        "test_cases/chosen_idx.json",
        "test_cases/test_cases.json",
        "mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json",
        "status_dict 3 servers / 11 tools",
        "inventory 5 tasks",
        "4 distinct names, 3 resolved",
        "Early=original (cumulative depth 0), Middle=iter2 (cumulative depth 2), Late=iter4 (cumulative depth 4)",
        "unresolved server name: \"ghost-server\"",
        "unresolved tool: @beta/calendar/legacy_event",
        "task_idx cross-check",
        "unmapped free_text_change: 1",
    ] {
        assert!(
            text.contains(needle),
            "validate output lacks {needle:?}:\n{text}"
        );
    }
    let brief = format!(
        "brief expected: {BRIEF_EXPECTED_SERVERS} servers / {BRIEF_EXPECTED_TOOLS} tools / {BRIEF_EXPECTED_TASKS} tasks; repository (authoritative): 3 servers / 11 tools / 5 tasks (differs; informational)"
    );
    assert!(text.contains(&brief), "{text}");
    assert!(!text.to_ascii_lowercase().contains("unconfirmed"), "{text}");
    // Every input file line carries a 64-hex SHA-256.
    let hashed = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.len() > 65 && t[..64].bytes().all(|b| b.is_ascii_hexdigit()) && &t[64..65] == " "
        })
        .count();
    assert_eq!(hashed, 7, "{text}");
    // Absolute paths never appear in the report.
    assert!(!text.contains(repo_root().to_str().unwrap()), "{text}");
    assert_eq!(report.inventory_task_count, 5);
    assert_eq!(report.task_ids, vec!["0".to_owned()]);
}

fn problems_of(result: harness_core::Result<harness_core::ValidationReport>) -> Vec<String> {
    match result {
        Err(HarnessError::Config(ConfigError::Invalid { problems })) => {
            problems.iter().map(ToString::to_string).collect()
        }
        other => panic!("expected config problems, got {other:?}"),
    }
}

#[test]
fn missing_env_vars_and_directories_are_listed_exactly() {
    let config = mini_config();

    let none: EnvLookup = Arc::new(|_| None);
    let problems = problems_of(validate_config_inputs(&config, &*none));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems[0].starts_with("inputs.catalog_env:"),
        "{problems:?}"
    );
    assert!(problems[0].contains("MCP_DATA_DIR is not set"));
    assert!(problems[0].contains("test_cases/status_dict.json"));
    assert!(problems[0].contains("mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json"));
    assert!(problems[1].starts_with("inputs.eval_env:"), "{problems:?}");
    assert!(problems[1].contains("EVAL_DATA_DIR is not set"));
    assert!(problems[1].contains("test_cases/test_cases.json"));
    assert!(problems[1].contains("test_cases/chosen_idx.json"));

    let gone: EnvLookup = Arc::new(|_| Some("/definitely/not/a/dir".to_owned()));
    let problems = problems_of(validate_config_inputs(&config, &*gone));
    assert!(
        problems
            .iter()
            .all(|p| p.contains("does not name an existing directory")),
        "{problems:?}"
    );

    // An existing directory with only some of the files lists exactly the missing ones.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("test_cases")).unwrap();
    fs::copy(
        mini_data().join("test_cases/status_dict.json"),
        tmp.path().join("test_cases/status_dict.json"),
    )
    .unwrap();
    let problems = problems_of(validate_config_inputs(&config, &*env_for(tmp.path())));
    let catalog = problems
        .iter()
        .find(|p| p.starts_with("inputs.catalog_env:"))
        .unwrap();
    assert!(
        !catalog.contains("status_dict.json"),
        "present file must not be listed: {catalog}"
    );
    assert!(
        catalog.contains("mutation/HYBRID/evolution.json"),
        "{catalog}"
    );
    let eval = problems
        .iter()
        .find(|p| p.starts_with("inputs.eval_env:"))
        .unwrap();
    assert!(
        eval.contains("test_cases/test_cases.json") && eval.contains("test_cases/chosen_idx.json")
    );
}

#[test]
fn config_stage_labels_must_exist_in_the_adapter() {
    let mut config = mini_config();
    config.versions.target = "iter9".to_owned();
    let problems = problems_of(validate_config_inputs(&config, &*env_for(&mini_data())));
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("versions.target:") && p.contains("iter9"))
    );
    let mut config = mini_config();
    config.tasks.ids = vec!["404".to_owned()];
    let problems = problems_of(validate_config_inputs(&config, &*env_for(&mini_data())));
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("tasks.ids:") && p.contains("404"))
    );
    // A selected task whose server is unresolved is reported, not run.
    let mut config = mini_config();
    config.tasks.ids = vec!["7".to_owned(), "0".to_owned()];
    assert!(validate_config_inputs(&config, &*env_for(&mini_data())).is_ok());
}

#[test]
fn run_matrix_smoke_over_the_mini_fixtures_hashes_real_inputs_in_the_manifest() {
    let config = mini_config();
    let out = tempfile::tempdir().unwrap();
    let opts = RunOptions::new(out.path())
        .with_clock(Arc::new(FixedClock::default()))
        .with_env(env_for(&mini_data()));
    let summary = run_matrix(&config, opts).expect("mini run");
    assert_eq!(summary.status, RunStatus::Ok, "{summary}");
    for path in [
        "manifest.toml",
        "events.jsonl",
        "summary.json",
        "kb/no_update-rep1.json",
        "kb/full_rewrite-rep1.json",
        "kb/selective_patch-rep1.json",
    ] {
        assert!(out.path().join(path).is_file(), "missing {path}");
    }

    // Manifest: the real input files, hashed, with source-relative names; no absolute paths.
    let manifest = fs::read_to_string(out.path().join("manifest.toml")).unwrap();
    let parsed: toml::Value = toml::from_str(&manifest).unwrap();
    let files = parsed["input_files"].as_array().unwrap();
    let inputs: Vec<(String, String)> = files
        .iter()
        .filter(|f| f["group"].as_str() == Some("inputs"))
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_owned(),
                f["sha256"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected_names: BTreeSet<String> = [
        "test_cases/status_dict.json",
        "test_cases/chosen_idx.json",
        "test_cases/test_cases.json",
        "mutation/HYBRID/evolution.json",
        "mutation/HYBRID/HYBRID/evolution.json",
        "mutation/HYBRID/HYBRID/HYBRID/evolution.json",
        "mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(
        inputs
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<BTreeSet<_>>(),
        expected_names
    );
    for (path, sha) in &inputs {
        let bytes = fs::read(mini_data().join(path)).unwrap();
        assert_eq!(&hex::encode(Sha256::digest(&bytes)), sha, "{path}");
    }
    assert!(
        !manifest.contains(repo_root().to_str().unwrap()),
        "no absolute paths in the manifest"
    );
    assert_eq!(parsed["inputs"]["adapter"].as_str(), Some("mcpevol"));
    assert!(
        parsed["inputs"]["release_id"]
            .as_str()
            .unwrap()
            .starts_with("mcpevol-")
    );
    assert_eq!(
        parsed["inputs"]["catalog_env"].as_str(),
        Some("MCP_DATA_DIR")
    );
    assert!(parsed["inputs"].get("root").is_none());
}

#[test]
fn fixture_adapter_tasks_still_load_without_server_ids() {
    let adapter =
        harness_core::source::FixtureAdapter::open(repo_root().join("fixtures/smoke")).unwrap();
    let inv = adapter.inventory().unwrap();
    assert!(inv.tasks.iter().all(|t| t.server_ids.is_empty()));
}
