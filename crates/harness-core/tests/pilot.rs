//! Pilot selection over the invented MCPEvol-Bench miniature in `fixtures/mcpevol-mini`. No
//! network and no reliance on the real benchmark clone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use harness_core::mcpevol::{McpEvolAdapter, McpEvolOptions};
use harness_core::pilot::{PilotReason, PilotRule, TaskFootprint, coverage_matrix, select_pilot};
use harness_core::source::SourceAdapter;
use harness_core::{ChangeClass, VersionStage};

fn mini_data() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/mcpevol-mini/data")
        .canonicalize()
        .expect("mini data")
}

fn open() -> McpEvolAdapter {
    let d = mini_data();
    McpEvolAdapter::open(&d, &d, McpEvolOptions::default()).expect("mini fixture opens")
}

#[test]
fn matrix_and_footprints_follow_the_adapter_inventory() {
    let adapter = open();
    let inventory = adapter.inventory().unwrap();
    let matrix = coverage_matrix(&inventory.tasks);
    println!("{}", matrix.to_table());
    assert_eq!(matrix.stage_tasks.values().sum::<usize>(), 5);
    assert_eq!(matrix.stage_unchanged[&VersionStage::Early], 1);
    assert_eq!(matrix.cell(VersionStage::Early, ChangeClass::Tool), 0);

    let footprints = adapter.task_footprints();
    assert_eq!(footprints.len(), 5);
    // Task 0 lists alpha-notes (create_note, list_notes) and calendar (add_event).
    assert_eq!(
        footprints["0"],
        TaskFootprint {
            servers: 2,
            tools: 3,
            resolved: true
        }
    );
    // Task 3 lists only alpha-notes/delete_note.
    assert_eq!(
        footprints["3"],
        TaskFootprint {
            servers: 1,
            tools: 1,
            resolved: true
        }
    );
}

#[test]
fn selection_is_deterministic_covers_cells_and_reports_shortfalls() {
    let adapter = open();
    let inventory = adapter.inventory().unwrap();
    let footprints = adapter.task_footprints();
    let rule = PilotRule {
        min_per_cell: 2,
        min_total: 4,
        max_total: 5,
        controls: 1,
    };
    let first = select_pilot(&inventory.tasks, footprints, rule);
    let second = select_pilot(&inventory.tasks, footprints, rule);
    assert_eq!(first, second);

    // Ids are unique, numerically ordered, within the bounds, and real inventory tasks.
    let mut sorted = first.ids.clone();
    sorted.sort_by_key(|id| id.parse::<u64>().unwrap());
    assert_eq!(first.ids, sorted);
    let unique: BTreeSet<&String> = first.ids.iter().collect();
    assert_eq!(unique.len(), first.ids.len());
    assert!((rule.min_total..=rule.max_total).contains(&first.ids.len()));
    for id in &first.ids {
        assert!(inventory.tasks.iter().any(|t| &t.task_id == id), "{id}");
    }

    // Task 7 names the unresolvable "ghost-server" and is left out, never selected.
    assert!(!footprints["7"].resolved);
    assert_eq!(first.excluded_unresolved, vec!["7".to_owned()]);
    assert!(!first.ids.contains(&"7".to_owned()));

    // The single Early task has no change class, so it is the control.
    assert_eq!(first.controls, 1);
    assert_eq!(first.reasons["3"], PilotReason::Control);

    // Every non-empty cell reached the target or is listed as short with its availability.
    let eligible: Vec<_> = inventory
        .tasks
        .iter()
        .filter(|t| t.task_id != "7")
        .cloned()
        .collect();
    let full = coverage_matrix(&eligible);
    for stage in [VersionStage::Middle, VersionStage::Late] {
        for class in [
            ChangeClass::Tool,
            ChangeClass::Parameter,
            ChangeClass::Description,
        ] {
            let available = full.cell(stage, class);
            let got = first.matrix.cell(stage, class);
            if available == 0 {
                assert_eq!(got, 0);
            } else if got < rule.min_per_cell {
                assert_eq!(got, available, "short cell must have used every member");
                assert!(
                    first
                        .short_cells
                        .iter()
                        .any(|c| c.stage == stage && c.class == class && c.available == available),
                    "short cell {stage:?} {class:?} not reported"
                );
            }
        }
    }
    let quoted: Vec<String> = first.ids.iter().map(|id| format!("\"{id}\"")).collect();
    assert_eq!(
        first.toml_ids_line(),
        format!("ids = [{}]", quoted.join(", "))
    );
}

#[test]
fn unreachable_minimum_total_is_reported_not_padded() {
    let adapter = open();
    let inventory = adapter.inventory().unwrap();
    let rule = PilotRule {
        min_total: 20,
        ..PilotRule::default()
    };
    let sel = select_pilot(&inventory.tasks, &BTreeMap::new(), rule);
    assert_eq!(sel.ids.len(), 5, "all five mini tasks, nothing invented");
    assert!(!sel.reached_min_total);
}
