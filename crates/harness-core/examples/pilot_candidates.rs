//! Prints the stage x change-class coverage matrix of the MCPEvol-Bench inventory and a
//! proposed pilot subset. Read-only, deterministic, no network.
//!
//! ```sh
//! MCP_DATA_DIR=<benchmark>/src/custom/data EVAL_DATA_DIR=<benchmark>/src/custom/data \
//!   cargo run -q -p harness-core --example pilot_candidates
//! ```
//!
//! The stage mapping is the adapter default (Early = original, Middle = iter2, Late = iter4).
//! The output is a PROPOSAL for the project owner. It freezes nothing.

use std::collections::BTreeSet;
use std::process::ExitCode;

use harness_core::mcpevol::{McpEvolAdapter, McpEvolOptions};
use harness_core::pilot::{
    CLASSES, PilotReason, PilotRule, STAGES, TaskFootprint, class_name, coverage_matrix,
    select_pilot, stage_name,
};

/// Left out of the proposal after a pilot run: their external servers crash.
const EXCLUDE_FOR_CAUSE: &[&str] = &["299", "401"];
use harness_core::source::SourceAdapter;

fn main() -> ExitCode {
    let (Some(catalog), Some(eval)) = (
        std::env::var_os("MCP_DATA_DIR"),
        std::env::var_os("EVAL_DATA_DIR"),
    ) else {
        eprintln!(
            "set MCP_DATA_DIR and EVAL_DATA_DIR to the benchmark's src/custom/data directory"
        );
        return ExitCode::from(2);
    };
    let adapter = match McpEvolAdapter::open(&catalog, &eval, McpEvolOptions::default()) {
        Ok(adapter) => adapter,
        Err(e) => {
            eprintln!("cannot open the benchmark data: {e}");
            return ExitCode::from(1);
        }
    };
    let inventory = match adapter.inventory() {
        Ok(inventory) => inventory,
        Err(e) => {
            eprintln!("cannot build the inventory: {e}");
            return ExitCode::from(1);
        }
    };
    let s = &adapter.options().stages;
    let d = adapter.stage_depths();
    println!("release: {}", adapter.release_id());
    println!(
        "stage mapping: early={} (depth {}), middle={} (depth {}), late={} (depth {})",
        s.early, d[0], s.middle, d[1], s.late, d[2]
    );
    println!("inventory tasks: {}", inventory.tasks.len());

    println!("\n1. Coverage matrix over all inventory tasks");
    println!("(a task counts once in every class it has; 'no-change' tasks have no class)");
    print!("{}", coverage_matrix(&inventory.tasks).to_table());
    println!("Early tasks have no change class by construction, so the Early row is controls.");

    let rule = PilotRule::default();
    let footprints = adapter.task_footprints();
    let exclude: BTreeSet<String> = EXCLUDE_FOR_CAUSE
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    let selection = select_pilot(&inventory.tasks, footprints, rule, &exclude);
    println!(
        "\n2. Proposed pilot ({} tasks), PROPOSED, NOT FROZEN",
        selection.ids.len()
    );
    println!(
        "rule: (0) leave out tasks that name an unresolved server or tool, and tasks whose external servers crash ({}); (1) cover every non-empty (stage, class) cell with >= {} tasks, scarcest cell first, \
         cheapest task first (fewer servers, then fewer tools, then lower numeric id); (2) add {} \
         Early control tasks (no change class), cheapest first; (3) while below {} tasks, give the \
         cell with the fewest selected tasks its cheapest unselected task; at most {} tasks",
        selection.excluded_for_cause.join(", "),
        rule.min_per_cell,
        rule.controls,
        rule.min_total,
        rule.max_total
    );
    println!("coverage matrix of the proposed pilot:");
    print!("{}", selection.matrix.to_table());
    if selection.short_cells.is_empty() {
        println!("every non-empty cell reached {} tasks", rule.min_per_cell);
    }
    for cell in &selection.short_cells {
        println!(
            "cell below target: {} x {}: {} selected, {} eligible task(s) available",
            stage_name(cell.stage),
            class_name(cell.class),
            cell.selected,
            cell.available
        );
    }
    let empty: Vec<String> = STAGES
        .iter()
        .flat_map(|st| CLASSES.iter().map(move |c| (*st, *c)))
        .filter(|(st, c)| coverage_matrix(&inventory.tasks).cell(*st, *c) == 0)
        .map(|(st, c)| format!("{} x {}", stage_name(st), class_name(c)))
        .collect();
    println!("empty cells in the whole inventory: {}", empty.join(", "));
    println!(
        "tasks left out because they name an unresolved server or tool: {} ({})",
        selection.excluded_unresolved.len(),
        selection.excluded_unresolved.join(", ")
    );
    println!(
        "tasks left out because their external servers crash: {} ({})",
        selection.excluded_for_cause.len(),
        selection.excluded_for_cause.join(", ")
    );
    println!(
        "controls selected: {}; total reached the minimum of {}: {}",
        selection.controls, rule.min_total, selection.reached_min_total
    );

    println!("\n3. TOML line for [tasks]");
    println!("{}", selection.toml_ids_line());

    println!("\n4. Per-task detail (servers and tools are those the task lists)");
    println!("id     stage   servers  tools  classes                    selected for");
    let mut total_servers = 0;
    let mut total_tools = 0;
    for id in &selection.ids {
        let Some(task) = inventory.tasks.iter().find(|t| &t.task_id == id) else {
            continue;
        };
        let f = footprints.get(id).copied().unwrap_or(TaskFootprint {
            servers: task.server_ids.len(),
            tools: 0,
            resolved: true,
        });
        total_servers += f.servers;
        total_tools += f.tools;
        let classes: Vec<&str> = task.change_classes.iter().map(|c| class_name(*c)).collect();
        let reason = match selection.reasons.get(id) {
            Some(PilotReason::Cell(st, c)) => format!("{} x {}", stage_name(*st), class_name(*c)),
            Some(PilotReason::Control) => "early control".to_owned(),
            Some(PilotReason::Balance(st, c)) => {
                format!("balance, {} x {}", stage_name(*st), class_name(*c))
            }
            Some(PilotReason::TopUp) => "top-up".to_owned(),
            None => String::new(),
        };
        println!(
            "{:<6} {:<7} {:>7}  {:>5}  {:<26} {}",
            id,
            stage_name(task.stage),
            f.servers,
            f.tools,
            if classes.is_empty() {
                "-".to_owned()
            } else {
                classes.join(",")
            },
            reason
        );
    }
    println!("sum over the pilot: {total_servers} server slots, {total_tools} tool slots");
    ExitCode::SUCCESS
}
