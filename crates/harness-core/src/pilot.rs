//! Coverage matrix and proposed pilot subset over an inventory of tasks.
//!
//! The functions here are pure and deterministic: the same tasks and footprints always give the
//! same matrix and the same ids. They never read files. `examples/pilot_candidates.rs` feeds them
//! the MCPEvol-Bench inventory.
//!
//! # Selection rule ([`select_pilot`])
//!
//! A task belongs to cell (stage, class) when `task.stage == stage` and `class` is in
//! `task.change_classes`. One task can belong to several cells of its stage row.
//!
//! 0. Tasks whose footprint says `resolved == false` (a server or tool the Tool KB cannot
//!    represent) are not eligible. They are listed in [`PilotSelection::excluded_unresolved`].
//!    Everything below uses eligible tasks only.
//! 1. Cells are the non-empty (stage, class) cells. They are processed scarcest first (fewest
//!    member tasks), ties broken by stage then class order.
//! 2. For each cell, tasks already selected count toward the target of
//!    [`PilotRule::min_per_cell`]. While the cell is short and has unselected members, the
//!    cheapest unselected member is added. Cheapest means fewer involved servers, then fewer
//!    involved tools, then lower numeric task id.
//! 3. Controls: tasks of stage Early that have no change class. [`PilotRule::controls`] of them
//!    (default 5) are added cheapest first, limited by availability and `max_total`.
//! 4. Balance: while the total is below [`PilotRule::min_total`], the non-empty cell with the
//!    fewest selected tasks (ties: the scarcer cell) receives its cheapest unselected member.
//!    A cell with no unselected member is skipped.
//! 5. If the total is still below `min_total`, the cheapest remaining tasks of any kind are
//!    added until it is reached or no task remains.
//!
//! A cell that ends below `min_per_cell` is reported in [`PilotSelection::short_cells`] with
//! the number of tasks available; nothing is dropped silently.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{ChangeClass, VersionStage};
use crate::source::TaskSpec;

/// Stages in matrix row order.
pub const STAGES: [VersionStage; 3] = [
    VersionStage::Early,
    VersionStage::Middle,
    VersionStage::Late,
];

/// Change classes in matrix column order.
pub const CLASSES: [ChangeClass; 3] = [
    ChangeClass::Tool,
    ChangeClass::Parameter,
    ChangeClass::Description,
];

/// Servers and tools a task itself lists. A cost proxy: more servers and tools mean a longer
/// agent run. It is not a measured cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskFootprint {
    /// Distinct server names the task lists.
    pub servers: usize,
    /// Distinct (server, tool) pairs the task lists.
    pub tools: usize,
    /// False when the task names a server that did not resolve to a catalog key, or a tool that
    /// exists in no reconstructed version. The Tool KB cannot represent such a task fully.
    pub resolved: bool,
}

/// Tasks per (stage, change class), plus per-stage totals.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoverageMatrix {
    /// Tasks counted in each cell. A task counts once in every class it has.
    pub cells: BTreeMap<(VersionStage, ChangeClass), usize>,
    /// Tasks per stage.
    pub stage_tasks: BTreeMap<VersionStage, usize>,
    /// Tasks per stage with no change class (controls when the stage is Early).
    pub stage_unchanged: BTreeMap<VersionStage, usize>,
}

impl CoverageMatrix {
    /// Count tasks in one cell.
    pub fn cell(&self, stage: VersionStage, class: ChangeClass) -> usize {
        self.cells.get(&(stage, class)).copied().unwrap_or(0)
    }

    /// Plain-text table, one row per stage.
    pub fn to_table(&self) -> String {
        let mut out = String::new();
        out.push_str("stage   tasks  tool  parameter  description  no-change\n");
        for stage in STAGES {
            out.push_str(&format!(
                "{:<7} {:>5}  {:>4}  {:>9}  {:>11}  {:>9}\n",
                stage_name(stage),
                self.stage_tasks.get(&stage).copied().unwrap_or(0),
                self.cell(stage, ChangeClass::Tool),
                self.cell(stage, ChangeClass::Parameter),
                self.cell(stage, ChangeClass::Description),
                self.stage_unchanged.get(&stage).copied().unwrap_or(0),
            ));
        }
        out
    }
}

/// Lowercase stage name.
pub fn stage_name(stage: VersionStage) -> &'static str {
    match stage {
        VersionStage::Early => "early",
        VersionStage::Middle => "middle",
        VersionStage::Late => "late",
    }
}

/// Lowercase class name.
pub fn class_name(class: ChangeClass) -> &'static str {
    match class {
        ChangeClass::Tool => "tool",
        ChangeClass::Parameter => "parameter",
        ChangeClass::Description => "description",
    }
}

/// Build the coverage matrix for `tasks`.
pub fn coverage_matrix(tasks: &[TaskSpec]) -> CoverageMatrix {
    let mut matrix = CoverageMatrix::default();
    for task in tasks {
        *matrix.stage_tasks.entry(task.stage).or_default() += 1;
        if task.change_classes.is_empty() {
            *matrix.stage_unchanged.entry(task.stage).or_default() += 1;
        }
        let classes: BTreeSet<ChangeClass> = task.change_classes.iter().copied().collect();
        for class in classes {
            *matrix.cells.entry((task.stage, class)).or_default() += 1;
        }
    }
    matrix
}

/// Parameters of the selection rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PilotRule {
    /// Target tasks per non-empty cell.
    pub min_per_cell: usize,
    /// Pilot size lower bound.
    pub min_total: usize,
    /// Pilot size upper bound.
    pub max_total: usize,
    /// Early control tasks to add (fewer when not enough exist).
    pub controls: usize,
}

impl Default for PilotRule {
    fn default() -> Self {
        Self {
            min_per_cell: 3,
            min_total: 20,
            max_total: 30,
            controls: 5,
        }
    }
}

/// A cell that could not reach the per-cell target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortCell {
    /// Stage of the cell.
    pub stage: VersionStage,
    /// Class of the cell.
    pub class: ChangeClass,
    /// Eligible tasks in the cell (unresolved tasks are not eligible).
    pub available: usize,
    /// Tasks of the pilot in the cell.
    pub selected: usize,
}

/// Why a task is in the pilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PilotReason {
    /// Added to reach a cell target.
    Cell(VersionStage, ChangeClass),
    /// Early task with no change class.
    Control,
    /// Added to balance cells while below the minimum total.
    Balance(VersionStage, ChangeClass),
    /// Added last, from any remaining task, to reach the minimum total.
    TopUp,
}

/// The proposed pilot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PilotSelection {
    /// Selected task ids in numeric order (non-numeric ids after numeric ones, as text).
    pub ids: Vec<String>,
    /// Why each id was added (the first reason that selected it).
    pub reasons: BTreeMap<String, PilotReason>,
    /// Number of Early control tasks selected.
    pub controls: usize,
    /// Cells that stayed below the per-cell target, with availability.
    pub short_cells: Vec<ShortCell>,
    /// Ids of tasks left out because they name an unresolved server or tool (numeric order).
    pub excluded_unresolved: Vec<String>,
    /// Whether the total reached `min_total`.
    pub reached_min_total: bool,
    /// Coverage matrix of the selected tasks.
    pub matrix: CoverageMatrix,
}

impl PilotSelection {
    /// `ids = ["1", "2"]` for a TOML `[tasks]` table.
    pub fn toml_ids_line(&self) -> String {
        let items: Vec<String> = self.ids.iter().map(|id| format!("{id:?}")).collect();
        format!("ids = [{}]", items.join(", "))
    }
}

fn id_key(id: &str) -> (u64, String) {
    (id.parse::<u64>().unwrap_or(u64::MAX), id.to_owned())
}

fn footprint_of(task: &TaskSpec, footprints: &BTreeMap<String, TaskFootprint>) -> TaskFootprint {
    footprints
        .get(&task.task_id)
        .copied()
        .unwrap_or_else(|| TaskFootprint {
            servers: task.server_ids.len().max(1),
            tools: 0,
            resolved: true,
        })
}

/// Select the proposed pilot. `footprints` is keyed by task id; a task without an entry is
/// costed as its listed servers and zero tools.
pub fn select_pilot(
    tasks: &[TaskSpec],
    footprints: &BTreeMap<String, TaskFootprint>,
    rule: PilotRule,
) -> PilotSelection {
    let cost = |t: &TaskSpec| {
        let f = footprint_of(t, footprints);
        (f.servers, f.tools, id_key(&t.task_id))
    };
    let in_cell = |t: &TaskSpec, stage: VersionStage, class: ChangeClass| {
        t.stage == stage && t.change_classes.contains(&class)
    };

    let all_tasks = tasks;
    let eligible: Vec<TaskSpec> = tasks
        .iter()
        .filter(|t| footprint_of(t, footprints).resolved)
        .cloned()
        .collect();
    let tasks = eligible.as_slice();
    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut reasons: BTreeMap<String, PilotReason> = BTreeMap::new();

    let mut cells: Vec<(usize, VersionStage, ChangeClass)> = Vec::new();
    for stage in STAGES {
        for class in CLASSES {
            let members = tasks.iter().filter(|t| in_cell(t, stage, class)).count();
            if members > 0 {
                cells.push((members, stage, class));
            }
        }
    }
    cells.sort();

    for (_, stage, class) in &cells {
        let mut members: Vec<&TaskSpec> = tasks
            .iter()
            .filter(|t| in_cell(t, *stage, *class))
            .collect();
        members.sort_by_key(|t| cost(t));
        let mut have = members
            .iter()
            .filter(|t| selected.contains(&t.task_id))
            .count();
        for task in members {
            if have >= rule.min_per_cell || selected.len() >= rule.max_total {
                break;
            }
            if selected.insert(task.task_id.clone()) {
                reasons.insert(task.task_id.clone(), PilotReason::Cell(*stage, *class));
                have += 1;
            }
        }
    }

    let mut controls: Vec<&TaskSpec> = tasks
        .iter()
        .filter(|t| t.stage == VersionStage::Early && t.change_classes.is_empty())
        .collect();
    controls.sort_by_key(|t| cost(t));
    let want = rule
        .controls
        .min(rule.max_total.saturating_sub(selected.len()));
    let mut added = 0;
    for task in controls {
        if added >= want {
            break;
        }
        if selected.insert(task.task_id.clone()) {
            reasons.insert(task.task_id.clone(), PilotReason::Control);
            added += 1;
        }
    }

    let selected_in = |selected: &BTreeSet<String>, stage: VersionStage, class: ChangeClass| {
        tasks
            .iter()
            .filter(|t| in_cell(t, stage, class) && selected.contains(&t.task_id))
            .count()
    };
    while selected.len() < rule.min_total.min(rule.max_total) {
        let mut order: Vec<(usize, usize, VersionStage, ChangeClass)> = cells
            .iter()
            .map(|(members, stage, class)| {
                (
                    selected_in(&selected, *stage, *class),
                    *members,
                    *stage,
                    *class,
                )
            })
            .collect();
        order.sort();
        let mut added_one = false;
        for (_, _, stage, class) in order {
            let pick = tasks
                .iter()
                .filter(|t| in_cell(t, stage, class) && !selected.contains(&t.task_id))
                .min_by_key(|t| cost(t));
            if let Some(task) = pick {
                selected.insert(task.task_id.clone());
                reasons.insert(task.task_id.clone(), PilotReason::Balance(stage, class));
                added_one = true;
                break;
            }
        }
        if !added_one {
            break;
        }
    }

    if selected.len() < rule.min_total {
        let mut rest: Vec<&TaskSpec> = tasks
            .iter()
            .filter(|t| !selected.contains(&t.task_id))
            .collect();
        rest.sort_by_key(|t| cost(t));
        for task in rest {
            if selected.len() >= rule.min_total {
                break;
            }
            selected.insert(task.task_id.clone());
            reasons.insert(task.task_id.clone(), PilotReason::TopUp);
        }
    }

    let mut ids: Vec<String> = selected.into_iter().collect();
    ids.sort_by_key(|id| id_key(id));
    let chosen: Vec<TaskSpec> = ids
        .iter()
        .filter_map(|id| tasks.iter().find(|t| &t.task_id == id).cloned())
        .collect();
    let matrix = coverage_matrix(&chosen);
    let short_cells = cells
        .iter()
        .filter_map(|(available, stage, class)| {
            let selected = matrix.cell(*stage, *class);
            (selected < rule.min_per_cell).then_some(ShortCell {
                stage: *stage,
                class: *class,
                available: *available,
                selected,
            })
        })
        .collect();
    let control_count = chosen
        .iter()
        .filter(|t| t.stage == VersionStage::Early && t.change_classes.is_empty())
        .count();
    let mut excluded_unresolved: Vec<String> = all_tasks
        .iter()
        .filter(|t| !footprint_of(t, footprints).resolved)
        .map(|t| t.task_id.clone())
        .collect();
    excluded_unresolved.sort_by_key(|id| id_key(id));
    let reached_min_total = ids.len() >= rule.min_total;
    PilotSelection {
        ids,
        reasons,
        controls: control_count,
        excluded_unresolved,
        short_cells,
        reached_min_total,
        matrix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, stage: VersionStage, classes: &[ChangeClass]) -> TaskSpec {
        TaskSpec {
            task_id: id.to_owned(),
            server_id: "s".to_owned(),
            server_ids: vec!["s".to_owned()],
            prompt: String::new(),
            stage,
            change_classes: classes.to_vec(),
        }
    }

    #[test]
    fn matrix_counts_a_task_in_each_of_its_classes() {
        let tasks = vec![
            task(
                "1",
                VersionStage::Middle,
                &[ChangeClass::Tool, ChangeClass::Description],
            ),
            task("2", VersionStage::Early, &[]),
        ];
        let m = coverage_matrix(&tasks);
        assert_eq!(m.cell(VersionStage::Middle, ChangeClass::Tool), 1);
        assert_eq!(m.cell(VersionStage::Middle, ChangeClass::Description), 1);
        assert_eq!(m.cell(VersionStage::Middle, ChangeClass::Parameter), 0);
        assert_eq!(m.stage_unchanged[&VersionStage::Early], 1);
        assert!(m.to_table().contains("middle"));
    }

    #[test]
    fn ids_sort_numerically_and_render_as_toml() {
        let tasks: Vec<TaskSpec> = ["10", "9", "100"]
            .iter()
            .map(|id| task(id, VersionStage::Early, &[]))
            .collect();
        let rule = PilotRule {
            min_total: 3,
            ..PilotRule::default()
        };
        let sel = select_pilot(&tasks, &BTreeMap::new(), rule);
        assert_eq!(sel.toml_ids_line(), r#"ids = ["9", "10", "100"]"#);
    }
}
