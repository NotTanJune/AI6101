//! Env-gated check against the real MCPEvol-Bench data. Runs only when MCP_DATA_DIR and
//! EVAL_DATA_DIR are set; otherwise it returns early. Never writes to the data directories.

use std::path::PathBuf;

use harness_core::diff::diff_kbs;
use harness_core::mcpevol::{McpEvolAdapter, McpEvolOptions, VERSION_LABELS};
use harness_core::source::SourceAdapter;

fn dirs() -> Option<(PathBuf, PathBuf)> {
    let catalog = std::env::var_os("MCP_DATA_DIR")?;
    let eval = std::env::var_os("EVAL_DATA_DIR")?;
    Some((catalog.into(), eval.into()))
}

#[test]
fn real_benchmark_loads_inventories_and_diffs() {
    let Some((catalog, eval)) = dirs() else {
        eprintln!("skipped: MCP_DATA_DIR and EVAL_DATA_DIR are not set");
        return;
    };
    let adapter = McpEvolAdapter::open(&catalog, &eval, McpEvolOptions::default())
        .expect("real benchmark opens");
    let report = adapter.report();
    println!("release: {}", adapter.release_id());
    for line in adapter.describe() {
        println!("{line}");
    }

    let inventory = adapter.inventory().expect("inventory");
    assert_eq!(
        inventory.tasks.len(),
        201,
        "chosen_idx.json lists 201 tasks"
    );
    assert_eq!(report.inventory_tasks, 201);
    println!(
        "inventory: {} servers, {} tasks, latest-version tools {}",
        inventory.servers.len(),
        inventory.tasks.len(),
        inventory.tool_count_latest()
    );

    let mut stage_counts = std::collections::BTreeMap::new();
    let mut class_counts = std::collections::BTreeMap::new();
    for task in &inventory.tasks {
        *stage_counts
            .entry(format!("{:?}", task.stage))
            .or_insert(0usize) += 1;
        for c in &task.change_classes {
            *class_counts.entry(format!("{c:?}")).or_insert(0usize) += 1;
        }
    }
    println!("task stages: {stage_counts:?}; tasks per change class: {class_counts:?}");

    for label in VERSION_LABELS {
        let kb = adapter.load_kb(label).expect("load_kb");
        println!("{label}: {} cards", kb.cards.len());
        assert_eq!(kb.server_version, label);
    }
    let original = adapter.load_kb("original").unwrap();
    for label in &VERSION_LABELS[1..] {
        let target = adapter.load_kb(label).unwrap();
        let diff = diff_kbs(&original, &target);
        let mut by_class = std::collections::BTreeMap::new();
        for c in &diff.changes {
            *by_class
                .entry(format!("{:?}/{:?}", c.class, c.kind))
                .or_insert(0usize) += 1;
        }
        println!(
            "diff original -> {label}: {} changes {by_class:?}",
            diff.changes.len()
        );
    }
    // Fidelity probe: the snapshot is closest to iter3 structurally. Count cards of iter3 whose
    // input schema equals the snapshot's, and print a few differences.
    let text = std::fs::read_to_string(catalog.join("test_cases/status_dict.json")).unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&text).unwrap();
    let iter3 = adapter.load_kb("iter3").unwrap();
    let (mut same, mut different, mut absent) = (0usize, 0usize, 0usize);
    let mut shown = 0;
    for card in &iter3.cards {
        let snap = snapshot[&card.server_id]["tools"]
            .as_array()
            .and_then(|t| t.iter().find(|t| t["name"] == card.tool_id));
        match snap {
            None => absent += 1,
            Some(t) if t["inputSchema"] == card.input_schema => same += 1,
            Some(_) => {
                different += 1;
                if shown < 5 {
                    shown += 1;
                    println!(
                        "iter3 schema differs from snapshot: {}/{}",
                        card.server_id, card.tool_id
                    );
                }
            }
        }
    }
    println!(
        "iter3 input schemas vs snapshot: {same} equal, {different} different, {absent} not in snapshot"
    );

    for u in &report.unmapped {
        println!(
            "unmapped d{} {} {:?} {}: {} | {}",
            u.depth,
            u.server,
            u.tool,
            u.section,
            u.reason,
            u.detail.chars().take(160).collect::<String>()
        );
    }
    for n in report.notes.iter().filter(|n| n.kind != "from_snapshot") {
        println!(
            "note {} d{} {} {:?}: {}",
            n.kind,
            n.depth,
            n.server,
            n.tool,
            n.detail.chars().take(160).collect::<String>()
        );
    }
}
