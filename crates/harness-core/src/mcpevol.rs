//! Source adapter for MCPEvol-Bench (<https://github.com/Octobrist/MCPEvol-Bench>).
//!
//! # Inputs
//!
//! Two `data` directories (they may be the same directory):
//!
//! * the catalog directory (`MCP_DATA_DIR`): `test_cases/status_dict.json` and
//!   `mutation/HYBRID{,/HYBRID,...}/evolution.json` (four nested files);
//! * the evaluation directory (`EVAL_DATA_DIR`): `test_cases/test_cases.json` and
//!   `test_cases/chosen_idx.json`.
//!
//! Nothing is ever written to either directory.
//!
//! # What the data really is (verified against commit 888c779a, see the module tests)
//!
//! * `status_dict.json` is NOT the original tool catalog. It is a snapshot of live servers in
//!   which most structural evolutions of depths 1 to 3 are already applied (added parameters
//!   and tools are present, deleted tools are absent) while description changes are not. The
//!   true original and the evolved definitions live in server bundles (HF dataset
//!   `anonymous2233/evol-servers`) that this adapter does not have.
//! * `task_idx` inside an evolution entry is NOT a `test_cases.json` idx (it matches a task that
//!   lists the mutated server in only a small minority of entries). It is never used to target
//!   tasks; see [`McpEvolReport::task_idx_matches`].
//!
//! # Replay
//!
//! The adapter therefore reconstructs versions from the snapshot, deterministically:
//!
//! 1. **Rewind** (default, [`McpEvolOptions::rewind_baseline`]): walking depth 4 down to 1, every
//!    mutation whose post-state is *evidenced* in the snapshot is undone (added parameter
//!    present: removed; added tool present: removed; description equal to the modified text:
//!    restored to the original text; required flag equal to the new state: restored). The
//!    removed definitions are remembered so the forward replay reinserts the snapshot's exact
//!    definition instead of a loose reconstruction. Mutations whose post-state is not evidenced
//!    are left alone. Removed parameters absent from the snapshot are restored only from what
//!    the mutation itself records (name, declared type or old description); a deleted tool that
//!    is absent from the snapshot cannot be restored and is reported.
//! 2. **Forward replay**: version `original` is the rewound baseline; `iterN` applies the
//!    evolution files `1..=N` oldest first (cumulative, as `get_all_evolution_status` does in
//!    the benchmark runner).
//!
//! Every card carries `provenance.source_pointer` listing the snapshot location and every
//! replayed mutation (`evolution:d2:<server>/<tool>#parameter_additions`). Whatever cannot be
//! mapped faithfully is recorded in [`McpEvolReport::unmapped`] and surfaced by `validate`;
//! nothing is guessed. Fidelity limits are listed in `docs` by the documentation step and in
//! [`McpEvolReport::notes`].
//!
//! # Name resolution
//!
//! Task and evolution server names sometimes differ from `status_dict` keys. A name resolves to
//! a key when (1) it is equal to the key, otherwise (2) exactly one key equals `@<name>` or ends
//! with `/<name>`. No match or several matches leaves the name unresolved; unresolved names are
//! reported and their servers are excluded from the KB, never invented.
//!
//! # Lineage
//!
//! `lineage_id = <server>::<original tool name>`. A tool introduced by a TOOL mutation gets its
//! own lineage (add). A deleted tool is a removal; `migration_target_tool` is documentation of
//! where behaviour moved to an existing tool and does NOT create lineage. Lineage is kept across
//! a rename only for the explicit `old_tool_name` + `new_tool_config` shape (the runner supports
//! it; the supplied data has no instance).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::error::{HarnessError, Result};
use crate::model::{
    ChangeClass, Provenance, TOOL_KB_SCHEMA_VERSION, ToolCard, ToolKb, VersionStage,
};
use crate::pilot::TaskFootprint;
use crate::source::{InputFile, Inventory, ServerInfo, SourceAdapter, TaskSpec, VersionInfo};

/// Servers the brief expected (EXPECTED metadata only; the repository's own counts are
/// authoritative and the difference is reported, never treated as an error).
pub const BRIEF_EXPECTED_SERVERS: usize = 123;
/// Tools the brief expected (EXPECTED metadata only, see [`BRIEF_EXPECTED_SERVERS`]).
pub const BRIEF_EXPECTED_TOOLS: usize = 1272;
/// Evaluation tasks the brief expected (EXPECTED metadata only, see [`BRIEF_EXPECTED_SERVERS`]).
pub const BRIEF_EXPECTED_TASKS: usize = 201;

/// Cumulative evolution depth of the deepest file.
pub const MAX_DEPTH: usize = 4;
/// Version labels in order: `original`, then `iter1` to `iter4`.
pub const VERSION_LABELS: [&str; MAX_DEPTH + 1] = ["original", "iter1", "iter2", "iter3", "iter4"];

/// Source-relative name of the tool snapshot.
pub const STATUS_DICT_FILE: &str = "test_cases/status_dict.json";
/// Source-relative name of the task pool.
pub const TEST_CASES_FILE: &str = "test_cases/test_cases.json";
/// Source-relative name of the evaluation selection.
pub const CHOSEN_IDX_FILE: &str = "test_cases/chosen_idx.json";

/// Source-relative name of the evolution file of cumulative depth `depth` (1 to 4).
pub fn evolution_file(depth: usize) -> String {
    let mut path = String::from("mutation");
    for _ in 0..depth {
        path.push_str("/HYBRID");
    }
    path.push_str("/evolution.json");
    path
}

/// Version label of cumulative depth `depth` (0 is `original`).
pub fn label_for_depth(depth: usize) -> Option<&'static str> {
    VERSION_LABELS.get(depth).copied()
}

/// Cumulative depth of a version label.
pub fn depth_for_label(label: &str) -> Option<usize> {
    VERSION_LABELS.iter().position(|l| *l == label)
}

/// Files that must exist in the catalog directory.
pub fn required_catalog_files() -> Vec<String> {
    let mut files = vec![STATUS_DICT_FILE.to_owned()];
    files.extend((1..=MAX_DEPTH).map(evolution_file));
    files
}

/// Files that must exist in the evaluation directory.
pub fn required_eval_files() -> Vec<String> {
    vec![TEST_CASES_FILE.to_owned(), CHOSEN_IDX_FILE.to_owned()]
}

/// Required files missing from `dir` (source-relative names).
pub fn missing_files(dir: &Path, required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|f| !dir.join(f).is_file())
        .cloned()
        .collect()
}

/// Which version label is the early, middle and late stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLabels {
    /// Label of the early stage.
    pub early: String,
    /// Label of the middle stage.
    pub middle: String,
    /// Label of the late stage.
    pub late: String,
}

impl Default for StageLabels {
    /// Project decision: Early = `original` (depth 0), Middle = `iter2` (cumulative depth 2),
    /// Late = `iter4` (cumulative depth 4). Configurable through `[versions.stages]`.
    fn default() -> Self {
        Self {
            early: "original".to_owned(),
            middle: "iter2".to_owned(),
            late: "iter4".to_owned(),
        }
    }
}

/// Adapter options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEvolOptions {
    /// Stage-to-label mapping.
    pub stages: StageLabels,
    /// Restrict the KB to the servers and tools of these task ids. `None` means all tasks of the
    /// inventory. Ids that are not in the inventory are ignored here and listed in
    /// [`McpEvolReport::unknown_scope_ids`].
    pub scope_task_ids: Option<Vec<String>>,
    /// Rewind the snapshot to the original before replaying (see the module docs). Turning it
    /// off replays evolutions onto the snapshot as is, which double-applies baked-in changes.
    pub rewind_baseline: bool,
}

impl Default for McpEvolOptions {
    fn default() -> Self {
        Self {
            stages: StageLabels::default(),
            scope_task_ids: None,
            rewind_baseline: true,
        }
    }
}

/// A mutation (or part of one) that could not be mapped faithfully and was NOT applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedEntry {
    /// Cumulative depth of the evolution file (1 to 4).
    pub depth: usize,
    /// Server (resolved key when it resolves, else the raw name).
    pub server: String,
    /// Tool the entry concerns, when known.
    pub tool: Option<String>,
    /// Section of the entry (`parameter_changes`, `auto_optimizations`, ...).
    pub section: String,
    /// Stable machine-readable reason.
    pub reason: String,
    /// Human-readable detail (original text where relevant).
    pub detail: String,
}

/// A replay decision worth reviewing that is not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayNote {
    /// Cumulative depth, 0 for baseline handling.
    pub depth: usize,
    /// Server.
    pub server: String,
    /// Tool, when known.
    pub tool: Option<String>,
    /// Kind: `rewound`, `already_applied`, `reconstructed`, `not_reconstructable`,
    /// `loose_type`, `loose_schema`, `base_mismatch`, `dropped_field`, `from_snapshot`.
    pub kind: &'static str,
    /// Detail.
    pub detail: String,
}

/// A task server name that did not resolve to a `status_dict` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedServer {
    /// Raw name as written in the data.
    pub name: String,
    /// Why it did not resolve.
    pub reason: String,
    /// Inventory tasks naming it.
    pub task_count: usize,
}

/// A tool a task names that exists in no reconstructed version of its server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedTool {
    /// Resolved server.
    pub server: String,
    /// Tool name as written in the task.
    pub tool: String,
    /// Inventory tasks naming it.
    pub task_count: usize,
}

/// Everything the adapter learned while opening the inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpEvolReport {
    /// Input files read, with hashes.
    pub files: Vec<InputFile>,
    /// Servers in `status_dict.json`.
    pub status_dict_servers: usize,
    /// Tools in `status_dict.json`.
    pub status_dict_tools: usize,
    /// Cases in `test_cases.json`.
    pub pool_tasks: usize,
    /// Tasks selected by `chosen_idx.json` (the inventory).
    pub inventory_tasks: usize,
    /// Inventory tasks inside the scope.
    pub scoped_tasks: usize,
    /// Scope ids that are not in the inventory.
    pub unknown_scope_ids: Vec<String>,
    /// Distinct server names the inventory tasks reference.
    pub referenced_servers: usize,
    /// Of those, the number that resolved to a `status_dict` key.
    pub resolved_servers: usize,
    /// Raw task name to resolved key, only where they differ.
    pub renamed_servers: BTreeMap<String, String>,
    /// Names that did not resolve.
    pub unresolved_servers: Vec<UnresolvedServer>,
    /// Task tool names with no card in any version.
    pub unresolved_tools: Vec<UnresolvedTool>,
    /// Servers in the scoped KB.
    pub scoped_servers: usize,
    /// Cards in the scoped KB per version label.
    pub scoped_cards: BTreeMap<String, usize>,
    /// Evolution entries per depth (index 0 is depth 1).
    pub entries_per_depth: [usize; MAX_DEPTH],
    /// Mutations not applied, with reasons.
    pub unmapped: Vec<UnmappedEntry>,
    /// Replay decisions worth reviewing.
    pub notes: Vec<ReplayNote>,
    /// Evolution entries whose `task_idx` equals the idx of a task listing the mutated server,
    /// out of all entries. Documents why `task_idx` is not used.
    pub task_idx_matches: (usize, usize),
}

// ---------------------------------------------------------------------------------------------
// Raw input shapes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawStatusServer {
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
    #[serde(default, rename = "outputSchema")]
    output_schema: Option<Value>,
}

/// Task `servers` object with declared order preserved (serde_json maps are sorted).
#[derive(Debug, Default)]
struct OrderedServers(Vec<(String, Vec<String>)>);

impl<'de> Deserialize<'de> for OrderedServers {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedServers;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object of server name to tool name list")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<OrderedServers, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Vec<String>>()? {
                    out.push((k, v));
                }
                Ok(OrderedServers(out))
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Debug, Deserialize)]
struct RawCase {
    idx: u64,
    #[serde(default)]
    question: String,
    #[serde(default)]
    servers: OrderedServers,
}

#[derive(Debug, Deserialize)]
struct RawChosen {
    samples: Vec<RawSample>,
}

#[derive(Debug, Deserialize)]
struct RawSample {
    idx: u64,
}

// ---------------------------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DescParam {
    name: String,
    /// `None` when the source records a null original (the parameter had no description).
    original: Option<String>,
    modified: String,
}

#[derive(Debug, Clone)]
struct ParamAdd {
    name: String,
    ty: Option<String>,
    required: bool,
    description: String,
}

#[derive(Debug, Clone)]
struct ParamRemove {
    name: String,
    ty: Option<String>,
    required: bool,
}

#[derive(Debug, Clone)]
enum ParamChange {
    Add {
        name: String,
        required: bool,
        description: String,
    },
    Remove {
        name: String,
        description_old: String,
        required: bool,
    },
    Required {
        name: String,
        old: String,
        new: String,
    },
    FreeText {
        name: String,
        kind: String,
        old: String,
        new: String,
    },
    Unrecognized(String),
}

#[derive(Debug, Clone)]
struct AutoOpt {
    tool: String,
    changes: String,
}

#[derive(Debug, Clone)]
enum Body {
    Desc {
        tool: String,
        tool_desc: Option<(String, String)>,
        params: Vec<DescParam>,
    },
    Param {
        tool: String,
        additions: Vec<ParamAdd>,
        changes: Vec<ParamChange>,
        removals: Vec<ParamRemove>,
        tool_desc: Option<(String, String)>,
    },
    ToolAdd {
        new_name: String,
        config: Option<Value>,
        old_name: Option<String>,
        auto_opts: Vec<AutoOpt>,
    },
    ToolDelete {
        name: String,
    },
}

#[derive(Debug, Clone)]
struct Mutation {
    depth: usize,
    /// Canonical server id: the resolved `status_dict` key, else the raw name.
    server: String,
    resolved: bool,
    class: ChangeClass,
    body: Body,
}

impl Mutation {
    fn tool(&self) -> Option<&str> {
        match &self.body {
            Body::Desc { tool, .. } | Body::Param { tool, .. } => Some(tool),
            Body::ToolAdd { new_name, .. } => Some(new_name),
            Body::ToolDelete { name } => Some(name),
        }
    }

    /// Tool this mutation introduces, if any.
    fn introduced(&self) -> Option<&str> {
        match &self.body {
            Body::ToolAdd { new_name, .. } => Some(new_name),
            _ => None,
        }
    }

    /// Does a task listing `tools` for this server count as involved? Same rule as
    /// `_is_server_involved` in the benchmark runner.
    fn involves(&self, tools: &[String]) -> bool {
        match &self.body {
            Body::Desc { tool, .. } | Body::Param { tool, .. } => tools.contains(tool),
            Body::ToolAdd { .. } => true,
            Body::ToolDelete { name } => tools.contains(name),
        }
    }

    fn rewind_pointer(&self, section: &str) -> String {
        format!(
            "rewind:d{}:{}/{}#{}",
            self.depth,
            self.server,
            self.tool().unwrap_or("-"),
            section
        )
    }

    fn pointer(&self, section: &str) -> String {
        format!(
            "evolution:d{}:{}/{}#{}",
            self.depth,
            self.server,
            self.tool().unwrap_or("-"),
            section
        )
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `(old, new)` descriptions; a null or missing old text means "no description" (empty).
fn desc_pair(v: &Value, old: &str, new: &str) -> Option<(String, String)> {
    let new = str_field(v, new)?;
    let old = match v.get(old) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return None,
    };
    Some((old, new))
}

fn parse_body(
    mutation_type: &str,
    entry: &Map<String, Value>,
) -> std::result::Result<Body, String> {
    let keys: Vec<&String> = entry
        .keys()
        .filter(|k| k.as_str() != "mutation_type" && k.as_str() != "task_idx")
        .collect();
    match mutation_type {
        "DESC" | "PARAM" => {
            if keys.len() != 1 {
                return Err(format!(
                    "expected exactly one tool key, found {}: {}",
                    keys.len(),
                    keys.iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let tool = keys[0].clone();
            let body = &entry[&tool];
            if !body.is_object() {
                return Err(format!("value of {tool:?} is not an object"));
            }
            if mutation_type == "DESC" {
                let tool_desc = body
                    .get("tool")
                    .and_then(|t| desc_pair(t, "original_description", "modified_description"))
                    .or_else(|| desc_pair(body, "original_description", "modified_description"));
                let params: Vec<DescParam> = body
                    .get("parameters")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                Some(DescParam {
                                    name: str_field(p, "parameter_name")?,
                                    original: str_field(p, "original_description"),
                                    modified: str_field(p, "modified_description")?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if tool_desc.is_none() && params.is_empty() {
                    return Err("DESC entry has neither a tool nor a parameters description".into());
                }
                Ok(Body::Desc {
                    tool,
                    tool_desc,
                    params,
                })
            } else {
                let additions: Vec<ParamAdd> = body
                    .get("parameter_additions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                Some(ParamAdd {
                                    name: str_field(p, "name")?,
                                    ty: str_field(p, "type"),
                                    required: p
                                        .get("required")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    description: str_field(p, "description").unwrap_or_default(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let changes: Vec<ParamChange> = body
                    .get("parameter_changes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(parse_change).collect())
                    .unwrap_or_default();
                let removals: Vec<ParamRemove> = body
                    .get("parameter_removals")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                Some(ParamRemove {
                                    name: str_field(p, "name")?,
                                    ty: str_field(p, "type"),
                                    required: p
                                        .get("required")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let tool_desc = body
                    .get("tool_description_change")
                    .and_then(|t| desc_pair(t, "old_description", "new_description"));
                if additions.is_empty()
                    && changes.is_empty()
                    && removals.is_empty()
                    && tool_desc.is_none()
                {
                    return Err("PARAM entry has no recognised section".into());
                }
                Ok(Body::Param {
                    tool,
                    additions,
                    changes,
                    removals,
                    tool_desc,
                })
            }
        }
        "TOOL" => {
            if let Some(name) = str_field_map(entry, "deleted_tool_name") {
                return Ok(Body::ToolDelete { name });
            }
            if let Some(config) = entry.get("new_tool_config").filter(|c| c.is_object()) {
                let new_name = str_field(config, "name")
                    .ok_or_else(|| "new_tool_config has no name".to_owned())?;
                return Ok(Body::ToolAdd {
                    new_name,
                    config: Some(config.clone()),
                    old_name: str_field_map(entry, "old_tool_name"),
                    auto_opts: Vec::new(),
                });
            }
            if let (Some(new_name), Some(opts)) = (
                str_field_map(entry, "new_tool_name"),
                entry.get("auto_optimizations").and_then(Value::as_array),
            ) {
                let auto_opts = opts
                    .iter()
                    .filter_map(|o| {
                        Some(AutoOpt {
                            tool: str_field(o, "tool_name")?,
                            changes: str_field(o, "changes").unwrap_or_default(),
                        })
                    })
                    .collect();
                return Ok(Body::ToolAdd {
                    new_name,
                    config: None,
                    old_name: None,
                    auto_opts,
                });
            }
            Err(format!(
                "TOOL entry has none of deleted_tool_name, new_tool_config, new_tool_name+auto_optimizations (keys: {})",
                keys.iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        other => Err(format!("unknown mutation_type {other:?}")),
    }
}

fn str_field_map(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn parse_change(v: &Value) -> ParamChange {
    let Some(name) = str_field(v, "name") else {
        return ParamChange::Unrecognized(v.to_string());
    };
    if let Some(action) = v.get("action").and_then(Value::as_str) {
        let required = v.get("required").and_then(Value::as_bool).unwrap_or(false);
        return match action {
            "add" => ParamChange::Add {
                name,
                required,
                description: str_field(v, "description_new").unwrap_or_default(),
            },
            "remove" => ParamChange::Remove {
                name,
                description_old: str_field(v, "description_old").unwrap_or_default(),
                required,
            },
            _ => ParamChange::Unrecognized(v.to_string()),
        };
    }
    if let (Some(kind), Some(old), Some(new)) = (
        str_field(v, "change_type"),
        str_field(v, "old_value"),
        str_field(v, "new_value"),
    ) {
        return if kind == "required" {
            ParamChange::Required { name, old, new }
        } else {
            ParamChange::FreeText {
                name,
                kind,
                old,
                new,
            }
        };
    }
    ParamChange::Unrecognized(v.to_string())
}

// ---------------------------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------------------------

fn props_of(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

fn has_property(schema: &Value, name: &str) -> bool {
    props_of(schema).is_some_and(|p| p.contains_key(name))
}

fn is_required(schema: &Value, name: &str) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name)))
}

fn set_required(schema: &mut Value, name: &str, required: bool) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    if required {
        let list = obj.entry("required").or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut()
            && !arr.iter().any(|v| v.as_str() == Some(name))
        {
            arr.push(Value::String(name.to_owned()));
        }
    } else if let Some(arr) = obj.get_mut("required").and_then(Value::as_array_mut) {
        arr.retain(|v| v.as_str() != Some(name));
    }
}

fn add_property(schema: &mut Value, name: &str, def: Value, required: bool) -> bool {
    let Some(obj) = schema.as_object_mut() else {
        return false;
    };
    let props = obj.entry("properties").or_insert_with(|| json!({}));
    let Some(props) = props.as_object_mut() else {
        return false;
    };
    props.insert(name.to_owned(), def);
    set_required(schema, name, required);
    true
}

fn remove_property(schema: &mut Value, name: &str) -> Option<Value> {
    let removed = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|p| p.remove(name))?;
    set_required(schema, name, false);
    Some(removed)
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn nested_paths(schema: &Value, name: &str, prefix: &str, out: &mut Vec<String>) {
    if let Some(props) = props_of(schema) {
        for (k, v) in props {
            let here = format!("{prefix}/properties/{}", escape_pointer(k));
            if k == name {
                out.push(here.clone());
            }
            nested_paths(v, name, &here, out);
        }
    }
    if let Some(items) = schema.get("items").filter(|i| i.is_object()) {
        nested_paths(items, name, &format!("{prefix}/items"), out);
    }
}

/// JSON pointer of the property `name`: top level first, else the single nested location.
fn locate_property(schema: &Value, name: &str) -> std::result::Result<String, String> {
    if has_property(schema, name) {
        return Ok(format!("/properties/{}", escape_pointer(name)));
    }
    let mut found = Vec::new();
    nested_paths(schema, name, "", &mut found);
    if found.is_empty() && name.contains('.') {
        // Dotted path such as `options.model`: walk properties (through array items too).
        let mut ptr = String::new();
        let mut cur = schema;
        for seg in name.split('.') {
            let direct = props_of(cur).filter(|p| p.contains_key(seg));
            let (next, step) = match direct {
                Some(p) => (&p[seg], format!("/properties/{}", escape_pointer(seg))),
                None => match cur.get("items").and_then(props_of).and_then(|p| p.get(seg)) {
                    Some(v) => (v, format!("/items/properties/{}", escape_pointer(seg))),
                    None => {
                        return Err(format!("parameter {name:?} not found in the input schema"));
                    }
                },
            };
            ptr.push_str(&step);
            cur = next;
        }
        return Ok(ptr);
    }
    match found.len() {
        0 => Err(format!("parameter {name:?} not found in the input schema")),
        1 => Ok(found.remove(0)),
        n => Err(format!("parameter {name:?} found at {n} nested locations")),
    }
}

fn set_description(value: &mut Value, text: &str) -> bool {
    match value.as_object_mut() {
        Some(obj) => {
            obj.insert("description".to_owned(), Value::String(text.to_owned()));
            true
        }
        None => false,
    }
}

fn parse_requiredness(text: &str) -> Option<bool> {
    let t = text.trim().to_ascii_lowercase();
    if t.starts_with("required") {
        Some(true)
    } else if t.starts_with("optional") {
        Some(false)
    } else {
        None
    }
}

fn quoted_values(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            let mut value = String::new();
            for d in chars.by_ref() {
                if d == c {
                    break;
                }
                value.push(d);
            }
            if !value.is_empty() {
                out.push(value);
            }
        }
    }
    out
}

fn scalar_type(t: &str) -> Option<&'static str> {
    match t {
        "string" => Some("string"),
        "number" => Some("number"),
        "int" | "integer" => Some("integer"),
        "bool" | "boolean" => Some("boolean"),
        "object" => Some("object"),
        "array" => Some("array"),
        "null" => Some("null"),
        _ => None,
    }
}

/// Conservative JSON Schema for a loose type string from `parameter_additions`. Returns the
/// property schema (without description) and, when the text was not a plain scalar type, a
/// note explaining the mapping. `None` as schema means the text could not be mapped at all.
fn loose_type_schema(raw: &str) -> (Option<Value>, Option<String>) {
    let t = raw.trim().to_ascii_lowercase();
    if let Some(s) = scalar_type(&t) {
        return (Some(json!({ "type": s })), None);
    }
    let note = |what: &str| Some(format!("declared type {raw:?} mapped to {what}"));
    match t.as_str() {
        "string[]" | "array<string>" | "array[string]" | "array of strings" => {
            return (
                Some(json!({ "type": "array", "items": { "type": "string" } })),
                note("an array of strings"),
            );
        }
        "array of objects" | "object[]" | "array<object>" => {
            return (
                Some(json!({ "type": "array", "items": { "type": "object" } })),
                note("an array of objects"),
            );
        }
        "record<string,string>" | "record<string, string>" => {
            return (
                Some(json!({ "type": "object", "additionalProperties": { "type": "string" } })),
                note("an object with string values"),
            );
        }
        _ => {}
    }
    if t.contains('|') && !t.contains('\'') && !t.contains('"') {
        let parts: Vec<Option<&str>> = t.split('|').map(|p| scalar_type(p.trim())).collect();
        if parts.iter().all(Option::is_some) {
            let types: Vec<Value> = parts.into_iter().flatten().map(Value::from).collect();
            return (Some(json!({ "type": types })), note("a type union"));
        }
    }
    if t.starts_with("enum") || t.starts_with("string (enum") || t.starts_with("string(enum") {
        let values = quoted_values(raw);
        return if values.is_empty() {
            (
                Some(json!({ "type": "string" })),
                note("a string; the enum values are not given"),
            )
        } else {
            (
                Some(json!({ "type": "string", "enum": values })),
                note("a string enum from the quoted values"),
            )
        };
    }
    (None, None)
}

/// A `new_tool_config.input_schema` is sometimes a bare property map. Wrap it; leave a proper
/// schema alone. The returned note is set when a wrap happened.
fn normalize_input_schema(schema: &Value) -> (Value, Option<String>) {
    match schema.as_object() {
        None => (
            json!({ "type": "object", "properties": {} }),
            Some("input_schema was not an object".to_owned()),
        ),
        Some(o) if o.contains_key("type") || o.contains_key("properties") => (schema.clone(), None),
        Some(o) if o.values().all(Value::is_object) => (
            json!({ "type": "object", "properties": schema.clone() }),
            Some(
                "input_schema was a bare property map; wrapped as properties of an object schema"
                    .to_owned(),
            ),
        ),
        Some(_) => (
            schema.clone(),
            Some("input_schema has an unrecognised shape; kept as is".to_owned()),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Replay state
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CardState {
    lineage_id: String,
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    /// Pointer into the snapshot, when the card came from it.
    base_ptr: Option<String>,
    /// Source file when the card did not come from the snapshot.
    built_in: Option<String>,
    /// Pointers of rewinds and replayed mutations that touched the card.
    touched: Vec<String>,
}

trait Touch {
    fn touch(&mut self, pointer: String);
}

impl Touch for Vec<String> {
    /// Record a pointer once, in order of first use.
    fn touch(&mut self, pointer: String) {
        if !self.contains(&pointer) {
            self.push(pointer);
        }
    }
}

type ServerState = Vec<CardState>;
type State = BTreeMap<String, ServerState>;

struct Engine {
    rewind: bool,
    notes: Vec<ReplayNote>,
    unmapped: Vec<UnmappedEntry>,
    memo_props: BTreeMap<(String, String, String), Value>,
    memo_cards: BTreeMap<(String, String), CardState>,
}

impl Engine {
    fn note(&mut self, m: &Mutation, kind: &'static str, detail: impl Into<String>) {
        self.notes.push(ReplayNote {
            depth: m.depth,
            server: m.server.clone(),
            tool: m.tool().map(str::to_owned),
            kind,
            detail: detail.into(),
        });
    }

    fn unmap(
        &mut self,
        m: &Mutation,
        tool: Option<&str>,
        section: &str,
        reason: &str,
        detail: impl Into<String>,
    ) {
        self.unmapped.push(UnmappedEntry {
            depth: m.depth,
            server: m.server.clone(),
            tool: tool.map(str::to_owned),
            section: section.to_owned(),
            reason: reason.to_owned(),
            detail: detail.into(),
        });
    }

    // ---- rewind ----

    fn undo(&mut self, m: &Mutation, state: &mut State) {
        if !m.resolved {
            return;
        }
        let Some(cards) = state.get_mut(&m.server) else {
            return;
        };
        match &m.body {
            Body::Desc {
                tool,
                tool_desc,
                params,
            } => {
                let Some(card) = cards.iter_mut().find(|c| &c.name == tool) else {
                    return;
                };
                if let Some((orig, modified)) = tool_desc
                    && card.description == *modified
                    && orig != modified
                {
                    card.description = orig.clone();
                    card.touched.touch(m.rewind_pointer("tool"));
                }
                for p in params {
                    if let Ok(ptr) = locate_property(&card.input_schema, &p.name)
                        && let Some(prop) = card.input_schema.pointer_mut(&ptr)
                        && prop.get("description").and_then(Value::as_str) == Some(&p.modified)
                        && p.original.as_deref() != Some(p.modified.as_str())
                    {
                        match &p.original {
                            Some(text) => {
                                set_description(prop, text);
                            }
                            None => {
                                if let Some(obj) = prop.as_object_mut() {
                                    obj.remove("description");
                                }
                            }
                        }
                        card.touched
                            .push(m.rewind_pointer(&format!("parameters[{}]", p.name)));
                    }
                }
            }
            Body::Param {
                tool,
                additions,
                changes,
                removals,
                tool_desc,
            } => {
                let Some(card) = cards.iter_mut().find(|c| &c.name == tool) else {
                    return;
                };
                let key = |p: &str| (m.server.clone(), tool.clone(), p.to_owned());
                for a in additions {
                    if let Some(def) = remove_property(&mut card.input_schema, &a.name) {
                        self.memo_props.insert(key(&a.name), def);
                        card.touched.touch(m.rewind_pointer("parameter_additions"));
                    }
                }
                for c in changes {
                    match c {
                        ParamChange::Add { name, .. } => {
                            if let Some(def) = remove_property(&mut card.input_schema, name) {
                                self.memo_props.insert(key(name), def);
                                card.touched.touch(m.rewind_pointer("parameter_changes"));
                            }
                        }
                        ParamChange::Required { name, old, new } => {
                            if let (Some(was), Some(now)) =
                                (parse_requiredness(old), parse_requiredness(new))
                                && was != now
                                && has_property(&card.input_schema, name)
                                && is_required(&card.input_schema, name) == now
                            {
                                set_required(&mut card.input_schema, name, was);
                                card.touched.touch(m.rewind_pointer("parameter_changes"));
                            }
                        }
                        _ => {}
                    }
                }
                if let Some((old, new)) = tool_desc
                    && card.description == *new
                    && old != new
                {
                    card.description = old.clone();
                    card.touched
                        .push(m.rewind_pointer("tool_description_change"));
                }
                // Removals that are already applied in the snapshot: restore from what the
                // mutation itself records, without inventing a description or constraints.
                let mut restores: Vec<(String, Value, bool, &'static str)> = Vec::new();
                for r in removals {
                    if !has_property(&card.input_schema, &r.name) {
                        let (schema, _) =
                            r.ty.as_deref()
                                .map(loose_type_schema)
                                .unwrap_or((None, None));
                        restores.push((
                            r.name.clone(),
                            schema.unwrap_or_else(|| json!({})),
                            r.required,
                            "parameter_removals",
                        ));
                    }
                }
                for c in changes {
                    if let ParamChange::Remove {
                        name,
                        description_old,
                        required,
                    } = c
                        && !has_property(&card.input_schema, name)
                    {
                        let mut def = json!({});
                        if !description_old.is_empty() {
                            set_description(&mut def, description_old);
                        }
                        restores.push((name.clone(), def, *required, "parameter_changes"));
                    }
                }
                for (name, mut def, required, section) in restores {
                    if let Some(memo) = self.memo_props.get(&key(&name)) {
                        def = memo.clone();
                    }
                    if add_property(&mut card.input_schema, &name, def, required) {
                        card.touched.touch(m.rewind_pointer(section));
                        self.notes.push(ReplayNote {
                            depth: m.depth,
                            server: m.server.clone(),
                            tool: Some(tool.clone()),
                            kind: "reconstructed",
                            detail: format!(
                                "parameter {name:?} was already removed in the snapshot; restored for earlier versions from the mutation record only (no type constraints beyond the declared type, no description unless recorded)"
                            ),
                        });
                    }
                }
            }
            Body::ToolAdd { new_name, .. } => {
                if let Some(pos) = cards.iter().position(|c| &c.name == new_name) {
                    let card = cards.remove(pos);
                    self.memo_cards
                        .insert((m.server.clone(), new_name.clone()), card);
                }
            }
            Body::ToolDelete { name } => {
                if !cards.iter().any(|c| &c.name == name) {
                    self.note(
                        m,
                        "not_reconstructable",
                        format!(
                            "tool {name:?} is already deleted in the snapshot; its definition is not available, so earlier versions lack it"
                        ),
                    );
                }
            }
        }
    }

    // ---- forward replay ----

    fn apply(&mut self, m: &Mutation, state: &mut State) {
        if !m.resolved {
            self.unmap(
                m,
                m.tool(),
                "entry",
                "server_not_in_status_dict",
                format!(
                    "server {:?} does not resolve to a status_dict key",
                    m.server
                ),
            );
            return;
        }
        match &m.body {
            Body::Desc {
                tool,
                tool_desc,
                params,
            } => {
                let Some(idx) = state
                    .get(&m.server)
                    .and_then(|c| c.iter().position(|c| &c.name == tool))
                else {
                    self.unmap(
                        m,
                        Some(tool),
                        "entry",
                        "tool_not_present",
                        format!("tool {tool:?} does not exist at depth {}", m.depth),
                    );
                    return;
                };
                let Some(mut card) = state.get(&m.server).and_then(|c| c.get(idx)).cloned() else {
                    return;
                };
                if let Some((orig, modified)) = tool_desc {
                    if card.description != *orig {
                        self.note(m, "base_mismatch", "description differs from the recorded original_description; the modified text is applied anyway");
                    }
                    card.description = modified.clone();
                    card.touched.touch(m.pointer("tool"));
                }
                for p in params {
                    match locate_property(&card.input_schema, &p.name) {
                        Ok(ptr) => {
                            if let Some(prop) = card.input_schema.pointer_mut(&ptr)
                                && set_description(prop, &p.modified)
                            {
                                card.touched
                                    .push(m.pointer(&format!("parameters[{}]", p.name)));
                            } else {
                                self.unmap(
                                    m,
                                    Some(tool),
                                    "parameters",
                                    "parameter_not_an_object",
                                    p.name.clone(),
                                );
                            }
                        }
                        Err(reason) => {
                            self.unmap(m, Some(tool), "parameters", "parameter_not_found", reason);
                        }
                    }
                }
                if let Some(slot) = state.get_mut(&m.server) {
                    slot[idx] = card;
                }
            }
            Body::Param {
                tool,
                additions,
                changes,
                removals,
                tool_desc,
            } => {
                let Some(idx) = state
                    .get(&m.server)
                    .and_then(|c| c.iter().position(|c| &c.name == tool))
                else {
                    self.unmap(
                        m,
                        Some(tool),
                        "entry",
                        "tool_not_present",
                        format!("tool {tool:?} does not exist at depth {}", m.depth),
                    );
                    return;
                };
                let Some(mut card) = state.get(&m.server).and_then(|c| c.get(idx)).cloned() else {
                    return;
                };
                for a in additions {
                    self.replay_add(
                        m,
                        &mut card,
                        &a.name,
                        a.ty.as_deref(),
                        a.required,
                        &a.description,
                        "parameter_additions",
                    );
                }
                for c in changes {
                    match c {
                        ParamChange::Add {
                            name,
                            required,
                            description,
                        } => {
                            self.replay_add(
                                m,
                                &mut card,
                                name,
                                None,
                                *required,
                                description,
                                "parameter_changes",
                            );
                        }
                        ParamChange::Remove { name, .. } => {
                            self.replay_remove(m, &mut card, name, "parameter_changes");
                        }
                        ParamChange::Required { name, old, new } => {
                            match (
                                parse_requiredness(new),
                                has_property(&card.input_schema, name),
                            ) {
                                (Some(now), true) => {
                                    if parse_requiredness(old) != Some(now)
                                        || is_required(&card.input_schema, name) != now
                                    {
                                        set_required(&mut card.input_schema, name, now);
                                        card.touched.touch(m.pointer("parameter_changes"));
                                    }
                                }
                                _ => self.unmap(
                                    m,
                                    Some(tool),
                                    "parameter_changes",
                                    "required_text_not_mapped",
                                    format!("{name}: {old:?} -> {new:?}"),
                                ),
                            }
                        }
                        ParamChange::FreeText {
                            name,
                            kind,
                            old,
                            new,
                        } => self.unmap(
                            m,
                            Some(tool),
                            "parameter_changes",
                            "free_text_change",
                            format!("{name} ({kind}): {old:?} -> {new:?}"),
                        ),
                        ParamChange::Unrecognized(text) => {
                            self.unmap(
                                m,
                                Some(tool),
                                "parameter_changes",
                                "unrecognized_change",
                                text.clone(),
                            );
                        }
                    }
                }
                for r in removals {
                    self.replay_remove(m, &mut card, &r.name, "parameter_removals");
                }
                if let Some((old, new)) = tool_desc {
                    if card.description != *old {
                        self.note(m, "base_mismatch", "description differs from tool_description_change.old_description; the new text is applied anyway");
                    }
                    card.description = new.clone();
                    card.touched.touch(m.pointer("tool_description_change"));
                }
                if let Some(slot) = state.get_mut(&m.server) {
                    slot[idx] = card;
                }
            }
            Body::ToolAdd {
                new_name,
                config,
                old_name,
                auto_opts,
            } => {
                for o in auto_opts {
                    self.unmap(
                        m,
                        Some(&o.tool),
                        "auto_optimizations",
                        "optimized_description_text_unavailable",
                        format!(
                            "the source records only prose about the change: {:?}",
                            o.changes
                        ),
                    );
                }
                let existing = state
                    .get(&m.server)
                    .and_then(|c| c.iter().position(|c| &c.name == new_name));
                if existing.is_some() {
                    self.note(
                        m,
                        "already_applied",
                        format!("tool {new_name:?} already exists; not added again"),
                    );
                    return;
                }
                let key = (m.server.clone(), new_name.clone());
                let mut card = if let Some(mut c) = self.memo_cards.get(&key).cloned() {
                    self.note(
                        m,
                        "from_snapshot",
                        format!("tool {new_name:?} taken from the snapshot definition"),
                    );
                    c.touched.touch(m.pointer("new_tool"));
                    c
                } else if let Some(cfg) = config {
                    let (schema, schema_note) =
                        normalize_input_schema(cfg.get("input_schema").unwrap_or(&Value::Null));
                    if let Some(n) = schema_note {
                        self.note(m, "loose_schema", n);
                    }
                    if let Some(extra) = cfg.get("annotations") {
                        self.unmap(
                            m,
                            Some(new_name),
                            "new_tool_config.annotations",
                            "unsupported_field",
                            extra.to_string(),
                        );
                    }
                    CardState {
                        lineage_id: format!("{}::{}", m.server, new_name),
                        name: new_name.clone(),
                        description: str_field(cfg, "description").unwrap_or_default(),
                        input_schema: schema,
                        output_schema: cfg.get("output_schema").filter(|v| v.is_object()).cloned(),
                        base_ptr: None,
                        built_in: Some(evolution_file(m.depth)),
                        touched: vec![m.pointer("new_tool_config")],
                    }
                } else {
                    self.unmap(
                        m,
                        Some(new_name),
                        "new_tool_name",
                        "tool_definition_unavailable",
                        "the source names the new tool but gives no definition and the snapshot does not contain it".to_owned(),
                    );
                    return;
                };
                if let Some(old) = old_name
                    && let Some(cards) = state.get_mut(&m.server)
                    && let Some(pos) = cards.iter().position(|c| &c.name == old)
                {
                    let removed = cards.remove(pos);
                    card.lineage_id = removed.lineage_id;
                    card.touched.touch(m.pointer("old_tool_name"));
                }
                if let Some(cards) = state.get_mut(&m.server) {
                    cards.push(card);
                }
            }
            Body::ToolDelete { name } => {
                let Some(cards) = state.get_mut(&m.server) else {
                    return;
                };
                if let Some(pos) = cards.iter().position(|c| &c.name == name) {
                    cards.remove(pos);
                } else {
                    self.note(
                        m,
                        "already_applied",
                        format!("tool {name:?} is already absent"),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_add(
        &mut self,
        m: &Mutation,
        card: &mut CardState,
        name: &str,
        ty: Option<&str>,
        required: bool,
        description: &str,
        section: &str,
    ) {
        if name.contains('[') || name.contains('.') {
            self.unmap(
                m,
                Some(&card.name.clone()),
                section,
                "nested_parameter_path",
                format!("parameter name {name:?} is a nested path"),
            );
            return;
        }
        if has_property(&card.input_schema, name) {
            self.note(
                m,
                "already_applied",
                format!("parameter {name:?} already exists; not added again"),
            );
            return;
        }
        let key = (m.server.clone(), card.name.clone(), name.to_owned());
        let memo = self.memo_props.get(&key).cloned();
        let def = if let Some(memo) = memo {
            self.note(
                m,
                "from_snapshot",
                format!("parameter {name:?} taken from the snapshot definition"),
            );
            memo
        } else {
            let mut def = match ty {
                Some(raw) => {
                    let (schema, note) = loose_type_schema(raw);
                    if let Some(n) = note {
                        self.note(m, "loose_type", format!("{name}: {n}"));
                    }
                    match schema {
                        Some(s) => s,
                        None => {
                            self.unmap(
                                m,
                                Some(&card.name.clone()),
                                section,
                                "type_text_not_mapped",
                                format!(
                                    "{name}: declared type {raw:?}; property added without a type"
                                ),
                            );
                            json!({})
                        }
                    }
                }
                None => json!({}),
            };
            if !description.is_empty() {
                set_description(&mut def, description);
            }
            def
        };
        if add_property(&mut card.input_schema, name, def, required) {
            card.touched.touch(m.pointer(section));
        } else {
            self.unmap(
                m,
                Some(&card.name.clone()),
                section,
                "schema_not_an_object",
                name.to_owned(),
            );
        }
    }

    fn replay_remove(&mut self, m: &Mutation, card: &mut CardState, name: &str, section: &str) {
        if remove_property(&mut card.input_schema, name).is_some() {
            card.touched.touch(m.pointer(section));
        } else {
            self.note(
                m,
                "already_applied",
                format!("parameter {name:?} is already absent"),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Name resolution
// ---------------------------------------------------------------------------------------------

/// Resolve a task or evolution server name to a `status_dict` key (module docs, "Name
/// resolution"). The error is the reason it did not resolve.
pub fn resolve_server_name(
    name: &str,
    keys: &BTreeSet<String>,
) -> std::result::Result<String, String> {
    if keys.contains(name) {
        return Ok(name.to_owned());
    }
    let scoped = format!("@{name}");
    let suffix = format!("/{name}");
    let candidates: Vec<&String> = keys
        .iter()
        .filter(|k| **k == scoped || k.ends_with(&suffix))
        .collect();
    match candidates.len() {
        0 => Err("no status_dict key equals, or ends with the name".to_owned()),
        1 => Ok(candidates[0].clone()),
        _ => Err(format!(
            "ambiguous: {}",
            candidates
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TaskRec {
    idx: u64,
    question: String,
    /// Canonical server id (resolved key, else raw) with the task's tool names.
    servers: Vec<(String, Vec<String>)>,
    resolved: Vec<bool>,
}

/// Read-only adapter over the MCPEvol-Bench data directories.
#[derive(Debug, Clone)]
pub struct McpEvolAdapter {
    release_id: String,
    stage_depths: [usize; 3],
    options: McpEvolOptions,
    /// Index = cumulative depth.
    states: Vec<State>,
    tasks: Vec<TaskSpec>,
    /// Per task id: servers and tools the task itself lists (a cost proxy).
    footprints: BTreeMap<String, TaskFootprint>,
    /// Scoped servers (resolved ids) and the tool names the KB covers.
    scope: BTreeMap<String, BTreeSet<String>>,
    report: McpEvolReport,
}

fn schema_err(what: &'static str, message: impl Into<String>) -> HarnessError {
    HarnessError::Schema {
        what,
        errors: vec![message.into()],
    }
}

fn read_input(dir: &Path, rel: &str, files: &mut Vec<InputFile>) -> Result<Vec<u8>> {
    let path = dir.join(rel);
    let bytes = fs::read(&path).map_err(|e| HarnessError::io(&path, e))?;
    files.push(InputFile {
        path: rel.to_owned(),
        sha256: hex::encode(Sha256::digest(&bytes)),
    });
    Ok(bytes)
}

/// Commit of the benchmark checkout the catalog directory belongs to. Only a checkout laid out
/// like MCPEvol-Bench (`<root>/src/custom/data` is the catalog directory) counts, so an
/// unrelated enclosing repository (for example this project's own) is never mistaken for the
/// benchmark's.
fn discover_commit(catalog_dir: &Path) -> Option<String> {
    let is_sha = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let catalog = catalog_dir.canonicalize().ok()?;
    for dir in catalog.ancestors() {
        let git = dir.join(".git");
        if !git.is_dir() {
            continue;
        }
        if dir
            .join("src")
            .join("custom")
            .join("data")
            .canonicalize()
            .ok()?
            != catalog
        {
            return None;
        }
        let head = fs::read_to_string(git.join("HEAD")).ok()?;
        let head = head.trim();
        if is_sha(head) {
            return Some(head.to_ascii_lowercase());
        }
        let reference = head.strip_prefix("ref: ")?.trim();
        if let Ok(text) = fs::read_to_string(git.join(reference)) {
            let sha = text.trim();
            if is_sha(sha) {
                return Some(sha.to_ascii_lowercase());
            }
        }
        if let Ok(packed) = fs::read_to_string(git.join("packed-refs")) {
            for line in packed.lines() {
                if let Some((sha, name)) = line.split_once(' ')
                    && name.trim() == reference
                    && is_sha(sha)
                {
                    return Some(sha.to_ascii_lowercase());
                }
            }
        }
        return None;
    }
    None
}

fn first_stage_at_or_after(depths: &[usize; 3], depth: usize) -> Option<VersionStage> {
    [
        VersionStage::Early,
        VersionStage::Middle,
        VersionStage::Late,
    ]
    .into_iter()
    .zip(depths.iter())
    .find(|(_, d)| **d >= depth)
    .map(|(s, _)| s)
}

impl McpEvolAdapter {
    /// Open and validate the inputs. `catalog_dir` and `eval_dir` are the benchmark `data`
    /// directories (see the module docs); they may be the same directory.
    pub fn open(
        catalog_dir: impl AsRef<Path>,
        eval_dir: impl AsRef<Path>,
        options: McpEvolOptions,
    ) -> Result<Self> {
        let catalog_dir = catalog_dir.as_ref();
        let eval_dir = eval_dir.as_ref();

        let mut stage_depths = [0usize; 3];
        for (slot, (name, label)) in stage_depths.iter_mut().zip([
            ("early", &options.stages.early),
            ("middle", &options.stages.middle),
            ("late", &options.stages.late),
        ]) {
            *slot = depth_for_label(label).ok_or_else(|| {
                schema_err(
                    "mcpevol stages",
                    format!(
                        "stage {name} maps to unknown version {label:?} (available: {})",
                        VERSION_LABELS.join(", ")
                    ),
                )
            })?;
        }
        if !(stage_depths[0] <= stage_depths[1] && stage_depths[1] <= stage_depths[2]) {
            return Err(schema_err(
                "mcpevol stages",
                "stage labels must not go backwards: early <= middle <= late",
            ));
        }

        let mut problems = missing_files(catalog_dir, &required_catalog_files())
            .into_iter()
            .map(|f| format!("catalog directory {} is missing {f}", catalog_dir.display()))
            .collect::<Vec<_>>();
        problems.extend(
            missing_files(eval_dir, &required_eval_files())
                .into_iter()
                .map(|f| format!("evaluation directory {} is missing {f}", eval_dir.display())),
        );
        if !problems.is_empty() {
            return Err(HarnessError::Schema {
                what: "mcpevol inputs",
                errors: problems,
            });
        }

        let mut files = Vec::new();
        let status_bytes = read_input(catalog_dir, STATUS_DICT_FILE, &mut files)?;
        let mut evolution_bytes = Vec::new();
        for depth in 1..=MAX_DEPTH {
            evolution_bytes.push(read_input(catalog_dir, &evolution_file(depth), &mut files)?);
        }
        let chosen_bytes = read_input(eval_dir, CHOSEN_IDX_FILE, &mut files)?;
        let cases_bytes = read_input(eval_dir, TEST_CASES_FILE, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let status: BTreeMap<String, RawStatusServer> = serde_json::from_slice(&status_bytes)
            .map_err(|e| schema_err("mcpevol status_dict.json", e.to_string()))?;
        let cases: Vec<RawCase> = serde_json::from_slice(&cases_bytes)
            .map_err(|e| schema_err("mcpevol test_cases.json", e.to_string()))?;
        let chosen: RawChosen = serde_json::from_slice(&chosen_bytes)
            .map_err(|e| schema_err("mcpevol chosen_idx.json", e.to_string()))?;
        let mut evolutions: Vec<Map<String, Value>> = Vec::new();
        for (i, bytes) in evolution_bytes.iter().enumerate() {
            let map: Map<String, Value> = serde_json::from_slice(bytes).map_err(|e| {
                schema_err("mcpevol evolution.json", format!("depth {}: {e}", i + 1))
            })?;
            evolutions.push(map);
        }

        let release_id = match discover_commit(catalog_dir) {
            Some(commit) => format!("mcpevol-{commit}"),
            None => {
                let mut hasher = Sha256::new();
                for f in &files {
                    hasher.update(f.path.as_bytes());
                    hasher.update([0]);
                    hasher.update(f.sha256.as_bytes());
                    hasher.update(*b"\n");
                }
                format!("mcpevol-{}", &hex::encode(hasher.finalize())[..12])
            }
        };

        Self::build(
            release_id,
            options,
            stage_depths,
            files,
            status,
            cases,
            chosen,
            evolutions,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        release_id: String,
        options: McpEvolOptions,
        stage_depths: [usize; 3],
        files: Vec<InputFile>,
        status: BTreeMap<String, RawStatusServer>,
        cases: Vec<RawCase>,
        chosen: RawChosen,
        evolutions: Vec<Map<String, Value>>,
    ) -> Result<Self> {
        let keys: BTreeSet<String> = status.keys().cloned().collect();
        let mut report = McpEvolReport {
            files,
            status_dict_servers: status.len(),
            status_dict_tools: status.values().map(|s| s.tools.len()).sum(),
            pool_tasks: cases.len(),
            ..McpEvolReport::default()
        };

        // Snapshot state.
        let mut base: State = BTreeMap::new();
        for (server, entry) in &status {
            let mut seen = BTreeSet::new();
            let mut cards = Vec::new();
            for (i, tool) in entry.tools.iter().enumerate() {
                if !seen.insert(tool.name.clone()) {
                    return Err(schema_err(
                        "mcpevol status_dict.json",
                        format!("server {server:?} lists tool {:?} twice", tool.name),
                    ));
                }
                let input_schema = match &tool.input_schema {
                    Some(v) if v.is_object() => v.clone(),
                    None => json!({ "type": "object", "properties": {} }),
                    Some(_) => {
                        return Err(schema_err(
                            "mcpevol status_dict.json",
                            format!("{server}/{}: inputSchema is not an object", tool.name),
                        ));
                    }
                };
                cards.push(CardState {
                    lineage_id: format!("{server}::{}", tool.name),
                    name: tool.name.clone(),
                    description: tool.description.clone().unwrap_or_default(),
                    input_schema,
                    output_schema: tool.output_schema.clone().filter(Value::is_object),
                    base_ptr: Some(format!("status_dict:/{}/tools/{i}", escape_pointer(server))),
                    built_in: None,
                    touched: Vec::new(),
                });
            }
            base.insert(server.clone(), cards);
        }

        // Tasks (inventory) with name resolution.
        let by_idx: BTreeMap<u64, &RawCase> = cases.iter().map(|c| (c.idx, c)).collect();
        let mut resolution: BTreeMap<String, std::result::Result<String, String>> = BTreeMap::new();
        let mut resolve = |name: &str| -> (String, bool) {
            let r = resolution
                .entry(name.to_owned())
                .or_insert_with(|| resolve_server_name(name, &keys))
                .clone();
            match r {
                Ok(key) => (key, true),
                Err(_) => (name.to_owned(), false),
            }
        };
        let mut missing_ids = Vec::new();
        let mut recs: Vec<TaskRec> = Vec::new();
        for sample in &chosen.samples {
            let Some(case) = by_idx.get(&sample.idx) else {
                missing_ids.push(sample.idx);
                continue;
            };
            let mut servers = Vec::new();
            let mut resolved = Vec::new();
            for (raw, tools) in &case.servers.0 {
                let (id, ok) = resolve(raw);
                servers.push((id, tools.clone()));
                resolved.push(ok);
            }
            recs.push(TaskRec {
                idx: case.idx,
                question: case.question.clone(),
                servers,
                resolved,
            });
        }
        if !missing_ids.is_empty() {
            return Err(schema_err(
                "mcpevol chosen_idx.json",
                format!(
                    "chosen idx values not present in test_cases.json: {}",
                    missing_ids
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        report.inventory_tasks = recs.len();

        // Server-name report (over the inventory tasks, using raw names).
        let mut raw_counts: BTreeMap<String, usize> = BTreeMap::new();
        for case in chosen.samples.iter().filter_map(|s| by_idx.get(&s.idx)) {
            for (raw, _) in &case.servers.0 {
                *raw_counts.entry(raw.clone()).or_default() += 1;
            }
        }
        report.referenced_servers = raw_counts.len();
        for (raw, count) in &raw_counts {
            match resolve_server_name(raw, &keys) {
                Ok(key) => {
                    report.resolved_servers += 1;
                    if key != *raw {
                        report.renamed_servers.insert(raw.clone(), key);
                    }
                }
                Err(reason) => report.unresolved_servers.push(UnresolvedServer {
                    name: raw.clone(),
                    reason,
                    task_count: *count,
                }),
            }
        }

        // Mutations.
        let mut mutations: Vec<Mutation> = Vec::new();
        let mut engine = Engine {
            rewind: options.rewind_baseline,
            notes: Vec::new(),
            unmapped: Vec::new(),
            memo_props: BTreeMap::new(),
            memo_cards: BTreeMap::new(),
        };
        let mut task_idx_matches = (0usize, 0usize);
        for (i, file) in evolutions.iter().enumerate() {
            let depth = i + 1;
            report.entries_per_depth[i] = file.len();
            for (raw_server, entry) in file {
                let (server, resolved) = {
                    match resolve_server_name(raw_server, &keys) {
                        Ok(k) => (k, true),
                        Err(_) => (raw_server.clone(), false),
                    }
                };
                let task_idx = entry.get("task_idx").and_then(Value::as_u64);
                task_idx_matches.1 += 1;
                if let Some(t) = task_idx.and_then(|t| by_idx.get(&t))
                    && t.servers
                        .0
                        .iter()
                        .any(|(n, _)| n == raw_server || n == &server)
                {
                    task_idx_matches.0 += 1;
                }
                let Some(obj) = entry.as_object() else {
                    engine.unmapped.push(UnmappedEntry {
                        depth,
                        server,
                        tool: None,
                        section: "entry".into(),
                        reason: "unrecognized_shape".into(),
                        detail: "entry is not an object".into(),
                    });
                    continue;
                };
                let mutation_type = obj
                    .get("mutation_type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let class = match mutation_type {
                    "TOOL" => ChangeClass::Tool,
                    "PARAM" => ChangeClass::Parameter,
                    "DESC" => ChangeClass::Description,
                    _ => {
                        engine.unmapped.push(UnmappedEntry {
                            depth,
                            server,
                            tool: None,
                            section: "entry".into(),
                            reason: "unrecognized_shape".into(),
                            detail: format!("unknown mutation_type {mutation_type:?}"),
                        });
                        continue;
                    }
                };
                match parse_body(mutation_type, obj) {
                    Ok(body) => mutations.push(Mutation {
                        depth,
                        server,
                        resolved,
                        class,
                        body,
                    }),
                    Err(detail) => engine.unmapped.push(UnmappedEntry {
                        depth,
                        server,
                        tool: None,
                        section: "entry".into(),
                        reason: "unrecognized_shape".into(),
                        detail,
                    }),
                }
            }
        }
        report.task_idx_matches = task_idx_matches;

        // States: baseline (rewound), then cumulative replay.
        let mut state0 = base;
        if engine.rewind {
            for depth in (1..=MAX_DEPTH).rev() {
                for m in mutations.iter().filter(|m| m.depth == depth) {
                    engine.undo(m, &mut state0);
                }
            }
        }
        let mut states = vec![state0];
        for depth in 1..=MAX_DEPTH {
            let mut next = states[depth - 1].clone();
            for m in mutations.iter().filter(|m| m.depth == depth) {
                engine.apply(m, &mut next);
            }
            states.push(next);
        }
        report.unmapped = engine.unmapped;
        report.notes = engine.notes;

        // Unresolved tools: named by an inventory task, present in no version.
        let mut unresolved_tools: BTreeMap<(String, String), usize> = BTreeMap::new();
        for rec in &recs {
            for ((server, tools), ok) in rec.servers.iter().zip(&rec.resolved) {
                if !ok {
                    continue;
                }
                for tool in tools {
                    let anywhere = states.iter().any(|s| {
                        s.get(server)
                            .is_some_and(|c| c.iter().any(|c| &c.name == tool))
                    });
                    if !anywhere {
                        *unresolved_tools
                            .entry((server.clone(), tool.clone()))
                            .or_default() += 1;
                    }
                }
            }
        }
        report.unresolved_tools = unresolved_tools
            .into_iter()
            .map(|((server, tool), task_count)| UnresolvedTool {
                server,
                tool,
                task_count,
            })
            .collect();

        // Task specs: stage and change classes from mutations that involve the task.
        let late_depth = stage_depths[2];
        let mut footprints: BTreeMap<String, TaskFootprint> = BTreeMap::new();
        let missing_tools: BTreeSet<(&str, &str)> = report
            .unresolved_tools
            .iter()
            .map(|u| (u.server.as_str(), u.tool.as_str()))
            .collect();
        let mut tasks = Vec::new();
        for rec in &recs {
            let mut stage: Option<VersionStage> = None;
            let mut classes: BTreeSet<ChangeClass> = BTreeSet::new();
            for (server, tools) in &rec.servers {
                for m in mutations
                    .iter()
                    .filter(|m| &m.server == server && m.depth <= late_depth && m.involves(tools))
                {
                    classes.insert(m.class);
                    if let Some(s) = first_stage_at_or_after(&stage_depths, m.depth) {
                        stage = Some(stage.map_or(s, |cur| cur.min(s)));
                    }
                }
            }
            let distinct_servers: BTreeSet<&str> =
                rec.servers.iter().map(|(s, _)| s.as_str()).collect();
            let distinct_tools: BTreeSet<(&str, &str)> = rec
                .servers
                .iter()
                .flat_map(|(s, tools)| tools.iter().map(move |t| (s.as_str(), t.as_str())))
                .collect();
            footprints.insert(
                rec.idx.to_string(),
                TaskFootprint {
                    servers: distinct_servers.len(),
                    tools: distinct_tools.len(),
                    resolved: rec.resolved.iter().all(|ok| *ok)
                        && !distinct_tools.iter().any(|p| missing_tools.contains(p)),
                },
            );
            let server_ids: Vec<String> = rec.servers.iter().map(|(s, _)| s.clone()).collect();
            let server_id = rec
                .servers
                .iter()
                .zip(&rec.resolved)
                .find(|(_, ok)| **ok)
                .map(|((s, _), _)| s.clone())
                .or_else(|| server_ids.first().cloned())
                .unwrap_or_default();
            tasks.push(TaskSpec {
                task_id: rec.idx.to_string(),
                server_id,
                server_ids,
                prompt: rec.question.clone(),
                stage: stage.unwrap_or(VersionStage::Early),
                change_classes: classes.into_iter().collect(),
            });
        }

        // Scope.
        let inventory_ids: BTreeSet<&str> = tasks.iter().map(|t| t.task_id.as_str()).collect();
        let scoped_ids: Option<BTreeSet<&str>> = options
            .scope_task_ids
            .as_ref()
            .map(|ids| ids.iter().map(String::as_str).collect());
        if let Some(ids) = &options.scope_task_ids {
            report.unknown_scope_ids = ids
                .iter()
                .filter(|id| !inventory_ids.contains(id.as_str()))
                .cloned()
                .collect();
        }
        let mut scope: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for rec in &recs {
            if let Some(ids) = &scoped_ids
                && !ids.contains(rec.idx.to_string().as_str())
            {
                continue;
            }
            report.scoped_tasks += 1;
            for ((server, tools), ok) in rec.servers.iter().zip(&rec.resolved) {
                if *ok {
                    scope
                        .entry(server.clone())
                        .or_default()
                        .extend(tools.iter().cloned());
                }
            }
        }
        for m in &mutations {
            if m.resolved
                && let Some(new) = m.introduced()
                && let Some(names) = scope.get_mut(&m.server)
            {
                names.insert(new.to_owned());
            }
        }
        report.scoped_servers = scope.len();

        let adapter = Self {
            release_id,
            stage_depths,
            options,
            states,
            tasks,
            footprints,
            scope,
            report,
        };
        let mut adapter = adapter;
        for (depth, label) in VERSION_LABELS.into_iter().enumerate() {
            let count = adapter.cards_at(depth, label).len();
            adapter.report.scoped_cards.insert(label.to_owned(), count);
        }
        Ok(adapter)
    }

    /// What the adapter learned while opening the inputs.
    pub fn report(&self) -> &McpEvolReport {
        &self.report
    }

    /// Options the adapter was opened with.
    pub fn options(&self) -> &McpEvolOptions {
        &self.options
    }

    /// Servers and tools each inventory task lists, keyed by task id. A cost proxy for pilot
    /// selection (see [`crate::pilot`]); it counts what the task names, not what mutations touch.
    pub fn task_footprints(&self) -> &BTreeMap<String, TaskFootprint> {
        &self.footprints
    }

    /// Cumulative depth of each stage (early, middle, late).
    pub fn stage_depths(&self) -> [usize; 3] {
        self.stage_depths
    }

    fn stage_of_label(&self, label: &str) -> Option<VersionStage> {
        let depth = depth_for_label(label)?;
        [
            VersionStage::Early,
            VersionStage::Middle,
            VersionStage::Late,
        ]
        .into_iter()
        .zip(self.stage_depths)
        .find(|(_, d)| *d == depth)
        .map(|(s, _)| s)
    }

    fn cards_at(&self, depth: usize, label: &str) -> Vec<ToolCard> {
        let mut cards = Vec::new();
        for (server, names) in &self.scope {
            let Some(state) = self.states[depth].get(server) else {
                continue;
            };
            for card in state {
                let lineage_tool = card
                    .lineage_id
                    .rsplit_once("::")
                    .map_or(card.name.as_str(), |(_, t)| t);
                if !names.contains(&card.name) && !names.contains(lineage_tool) {
                    continue;
                }
                let (source_file, mut pointer) = match (&card.base_ptr, &card.built_in) {
                    (Some(p), _) => (STATUS_DICT_FILE.to_owned(), vec![p.clone()]),
                    (None, Some(f)) => (f.clone(), Vec::new()),
                    (None, None) => (STATUS_DICT_FILE.to_owned(), Vec::new()),
                };
                pointer.extend(card.touched.iter().cloned());
                cards.push(ToolCard {
                    lineage_id: card.lineage_id.clone(),
                    server_id: server.clone(),
                    tool_id: card.name.clone(),
                    name: card.name.clone(),
                    server_version: label.to_owned(),
                    description: card.description.clone(),
                    input_schema: card.input_schema.clone(),
                    output_schema: card.output_schema.clone(),
                    usage_rules: Vec::new(),
                    provenance: Provenance {
                        release_id: self.release_id.clone(),
                        source_file,
                        source_pointer: if pointer.is_empty() {
                            None
                        } else {
                            Some(pointer.join("; "))
                        },
                    },
                });
            }
        }
        cards
    }
}

impl SourceAdapter for McpEvolAdapter {
    fn release_id(&self) -> &str {
        &self.release_id
    }

    fn version_labels(&self) -> Vec<String> {
        VERSION_LABELS.iter().map(|l| (*l).to_owned()).collect()
    }

    fn input_files(&self) -> Vec<InputFile> {
        self.report.files.clone()
    }

    fn inventory(&self) -> Result<Inventory> {
        let servers = self
            .scope
            .keys()
            .map(|server| ServerInfo {
                server_id: server.clone(),
                versions: (0..=MAX_DEPTH)
                    .map(|depth| {
                        let label = VERSION_LABELS[depth];
                        let names = &self.scope[server];
                        let tool_count = self.states[depth]
                            .get(server)
                            .map(|cards| {
                                cards
                                    .iter()
                                    .filter(|c| {
                                        let lt = c
                                            .lineage_id
                                            .rsplit_once("::")
                                            .map_or(c.name.as_str(), |(_, t)| t);
                                        names.contains(&c.name) || names.contains(lt)
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        VersionInfo {
                            label: label.to_owned(),
                            stage: self.stage_of_label(label),
                            tool_count,
                        }
                    })
                    .collect(),
            })
            .collect();
        Ok(Inventory {
            release_id: self.release_id.clone(),
            servers,
            tasks: self.tasks.clone(),
        })
    }

    fn load_kb(&self, version: &str) -> Result<ToolKb> {
        let Some(depth) = depth_for_label(version) else {
            return Err(schema_err(
                "mcpevol version",
                format!(
                    "unknown version {version:?} (available: {})",
                    VERSION_LABELS.join(", ")
                ),
            ));
        };
        Ok(ToolKb {
            schema_version: TOOL_KB_SCHEMA_VERSION.to_owned(),
            kb_id: format!("kb-{}-{version}", self.release_id),
            release_id: self.release_id.clone(),
            server_version: version.to_owned(),
            cards: self.cards_at(depth, version),
        })
    }

    fn describe(&self) -> Vec<String> {
        let r = &self.report;
        let s = &self.options.stages;
        let d = self.stage_depths;
        let mut lines = Vec::new();
        lines.push("mcpevol input files (source-relative, SHA-256):".to_owned());
        for f in &r.files {
            lines.push(format!("  {}  {}", f.sha256, f.path));
        }
        lines.push(format!(
            "counts: status_dict {} servers / {} tools; test_cases pool {}; inventory {} tasks (chosen_idx); {} task(s) in scope",
            r.status_dict_servers, r.status_dict_tools, r.pool_tasks, r.inventory_tasks, r.scoped_tasks
        ));
        lines.push(format!(
            "servers referenced by the {} inventory tasks: {} distinct names, {} resolved ({} by suffix rule), {} unresolved",
            r.inventory_tasks,
            r.referenced_servers,
            r.resolved_servers,
            r.renamed_servers.len(),
            r.unresolved_servers.len()
        ));
        lines.push(format!(
            "scope: {} distinct server key(s); cards per version: {}",
            r.scoped_servers,
            r.scoped_cards
                .iter()
                .map(|(l, n)| format!("{l}={n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        lines.push(format!(
            "stage mapping (project decision, configurable via [versions.stages]): Early={} (cumulative depth {}), Middle={} (cumulative depth {}), Late={} (cumulative depth {})",
            s.early, d[0], s.middle, d[1], s.late, d[2]
        ));
        let differs = r.status_dict_servers != BRIEF_EXPECTED_SERVERS
            || r.status_dict_tools != BRIEF_EXPECTED_TOOLS
            || r.inventory_tasks != BRIEF_EXPECTED_TASKS;
        lines.push(format!(
            "brief expected: {BRIEF_EXPECTED_SERVERS} servers / {BRIEF_EXPECTED_TOOLS} tools / {BRIEF_EXPECTED_TASKS} tasks; repository (authoritative): {} servers / {} tools / {} tasks{}",
            r.status_dict_servers,
            r.status_dict_tools,
            r.inventory_tasks,
            if differs { " (differs; informational)" } else { " (matches)" }
        ));
        for u in &r.unresolved_servers {
            lines.push(format!(
                "unresolved server name: {:?} ({} task(s)): {}",
                u.name, u.task_count, u.reason
            ));
        }
        for (raw, key) in &r.renamed_servers {
            lines.push(format!(
                "server name resolved by suffix rule: {raw:?} -> {key:?}"
            ));
        }
        for t in &r.unresolved_tools {
            lines.push(format!(
                "unresolved tool: {}/{} ({} task(s)); present in no reconstructed version",
                t.server, t.tool, t.task_count
            ));
        }
        lines.push(format!(
            "evolution: entries per depth {:?}; {} unmapped item(s); {} replay note(s)",
            r.entries_per_depth,
            r.unmapped.len(),
            r.notes.len()
        ));
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for u in &r.unmapped {
            *by_reason.entry(u.reason.as_str()).or_default() += 1;
        }
        for (reason, n) in by_reason {
            lines.push(format!("  unmapped {reason}: {n}"));
        }
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for n in &r.notes {
            *by_kind.entry(n.kind).or_default() += 1;
        }
        for (kind, n) in by_kind {
            lines.push(format!("  note {kind}: {n}"));
        }
        lines.push(format!(
            "task_idx cross-check: {} of {} evolution entries point at a task that lists the mutated server; task_idx is not used to target tasks",
            r.task_idx_matches.0, r.task_idx_matches.1
        ));
        lines.push(format!(
            "baseline: status_dict is a snapshot with evolutions already applied, not the original; {}",
            if self.options.rewind_baseline {
                "evidenced structural changes are rewound before replay (approximation)"
            } else {
                "rewind is disabled, evolutions are replayed onto the snapshot as is"
            }
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evolution_file_nests_one_level_per_depth() {
        assert_eq!(evolution_file(1), "mutation/HYBRID/evolution.json");
        assert_eq!(
            evolution_file(4),
            "mutation/HYBRID/HYBRID/HYBRID/HYBRID/evolution.json"
        );
    }

    #[test]
    fn labels_and_depths_round_trip() {
        for (i, l) in VERSION_LABELS.iter().enumerate() {
            assert_eq!(depth_for_label(l), Some(i));
            assert_eq!(label_for_depth(i), Some(*l));
        }
        assert_eq!(depth_for_label("iter5"), None);
    }

    #[test]
    fn loose_types_map_conservatively() {
        assert_eq!(
            loose_type_schema("bool").0,
            Some(json!({"type": "boolean"}))
        );
        assert_eq!(loose_type_schema("int").0, Some(json!({"type": "integer"})));
        let (s, note) = loose_type_schema("array of objects");
        assert_eq!(
            s,
            Some(json!({"type": "array", "items": {"type": "object"}}))
        );
        assert!(note.is_some());
        let (s, _) = loose_type_schema("enum['SOLVED', 'ATTEMPTED']");
        assert_eq!(
            s,
            Some(json!({"type": "string", "enum": ["SOLVED", "ATTEMPTED"]}))
        );
        let (s, note) = loose_type_schema("string (enum)");
        assert_eq!(s, Some(json!({"type": "string"})));
        assert!(note.unwrap().contains("not given"));
        assert_eq!(
            loose_type_schema("string|object").0,
            Some(json!({"type": ["string", "object"]}))
        );
        assert_eq!(loose_type_schema("weird thing").0, None);
    }

    #[test]
    fn requiredness_parses_only_explicit_words() {
        assert_eq!(parse_requiredness("optional string"), Some(false));
        assert_eq!(parse_requiredness("required boolean"), Some(true));
        assert_eq!(parse_requiredness("z.enum([\"svg\"])"), None);
    }

    #[test]
    fn bare_property_maps_are_wrapped_and_proper_schemas_kept() {
        let (v, n) = normalize_input_schema(&json!({"ip": {"type": "string"}}));
        assert_eq!(
            v,
            json!({"type": "object", "properties": {"ip": {"type": "string"}}})
        );
        assert!(n.is_some());
        let proper = json!({"type": "object", "properties": {}});
        assert_eq!(normalize_input_schema(&proper), (proper.clone(), None));
    }

    #[test]
    fn nested_parameter_lookup_needs_a_unique_location() {
        let schema = json!({"type": "object", "properties": {"sources": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}}}}}});
        assert_eq!(
            locate_property(&schema, "path").unwrap(),
            "/properties/sources/items/properties/path"
        );
        assert!(locate_property(&schema, "missing").is_err());
    }

    #[test]
    fn server_names_resolve_by_exact_then_unique_suffix() {
        let keys: BTreeSet<String> = ["@scope/server-memory", "plain", "@a/x-mcp", "@b/x-mcp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(resolve_server_name("plain", &keys).unwrap(), "plain");
        assert_eq!(
            resolve_server_name("server-memory", &keys).unwrap(),
            "@scope/server-memory"
        );
        assert_eq!(
            resolve_server_name("scope/server-memory", &keys).unwrap(),
            "@scope/server-memory"
        );
        assert!(
            resolve_server_name("x-mcp", &keys)
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(resolve_server_name("nope", &keys).is_err());
    }
}
