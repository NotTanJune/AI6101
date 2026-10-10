//! Source adapter boundary: turns a benchmark release (or a local fixture) into
//! normalized Tool KBs and an inventory of servers, versions and tasks.
//!
//! The types below are shared with the TUI. `FixtureAdapter` serves the offline smoke fixtures;
//! `crate::mcpevol::McpEvolAdapter` reads the supplied MCPEvol-Bench release.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{ChangeClass, ToolKb, VersionStage};

/// One evaluation task as exposed by the benchmark release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub task_id: String,
    pub server_id: String,
    /// Every server the task involves, in declared order. Empty means "only `server_id`"
    /// (older fixtures); the MCPEvol adapter always fills it.
    #[serde(default)]
    pub server_ids: Vec<String>,
    /// Original user request. Read-only ground truth; never edited by the harness.
    pub prompt: String,
    /// Server version stage this task is evaluated at.
    pub stage: VersionStage,
    /// Change classes that the release metadata associates with this task.
    pub change_classes: Vec<ChangeClass>,
}

/// One version of a server as listed in the inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub label: String,
    pub stage: Option<VersionStage>,
    pub tool_count: usize,
}

/// One server and the versions the release provides for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_id: String,
    pub versions: Vec<VersionInfo>,
}

/// Everything the operator can browse and select from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub release_id: String,
    pub servers: Vec<ServerInfo>,
    pub tasks: Vec<TaskSpec>,
}

impl Inventory {
    /// Total number of tools across the latest listed version of every server.
    pub fn tool_count_latest(&self) -> usize {
        self.servers
            .iter()
            .filter_map(|s| s.versions.last())
            .map(|v| v.tool_count)
            .sum()
    }
}

/// One input file an adapter read, for the run manifest. `path` is relative to the input root
/// (never absolute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
    /// Source-relative path, `/` separated.
    pub path: String,
    /// SHA-256 of the file bytes, lowercase hex.
    pub sha256: String,
}

/// Reads a benchmark release into normalized structures. Implementations are
/// read-only: the supplied catalog and evaluation data are immutable inputs.
pub trait SourceAdapter: Send {
    fn release_id(&self) -> &str;
    fn inventory(&self) -> Result<Inventory>;
    /// Normalized Tool KB for one version label (all servers in scope).
    fn load_kb(&self, version: &str) -> Result<ToolKb>;
    /// Version labels the adapter can load, in version order.
    fn version_labels(&self) -> Vec<String>;
    /// Input files to hash into the run manifest. Empty means "hash the config's fixture root".
    fn input_files(&self) -> Vec<InputFile> {
        Vec::new()
    }
    /// Extra lines for `harness validate` (counts, mappings, discrepancies).
    fn describe(&self) -> Vec<String> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------------------------
// FixtureAdapter
// ---------------------------------------------------------------------------------------------

/// One entry of a fixture `lineage.json`: tool `tool_id` of `server_id`, as it appears in KB
/// version `from_version` and in every later version that still lists that tool id, belongs to
/// lineage `lineage_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineageEntry {
    pub server_id: String,
    pub tool_id: String,
    pub from_version: String,
    pub lineage_id: String,
}

/// Offline adapter over checked-in fixtures.
///
/// Layout under the fixture root:
///
/// * `kb/<version>.json`: one Tool KB (`tool-kb/1`) per version label; the file stem must equal
///   the KB's `server_version`.
/// * `tasks.json`: a JSON array of [`TaskSpec`].
/// * `lineage.json` (optional): a JSON array of [`LineageEntry`], the explicit rename map. It is
///   authoritative: it overrides the `lineage_id` written in a KB file.
///
/// Version order is `early`, `middle`, `late`, then any other label alphabetically.
#[derive(Debug, Clone)]
pub struct FixtureAdapter {
    root: std::path::PathBuf,
    release_id: String,
    versions: Vec<(String, ToolKb)>,
    tasks: Vec<TaskSpec>,
}

fn fixture_error(what: &'static str, message: impl Into<String>) -> crate::error::HarnessError {
    crate::error::HarnessError::Schema {
        what,
        errors: vec![message.into()],
    }
}

fn stage_of(label: &str) -> Option<VersionStage> {
    match label {
        "early" => Some(VersionStage::Early),
        "middle" => Some(VersionStage::Middle),
        "late" => Some(VersionStage::Late),
        _ => None,
    }
}

fn version_rank(label: &str) -> (u8, String) {
    let rank = match stage_of(label) {
        Some(VersionStage::Early) => 0,
        Some(VersionStage::Middle) => 1,
        Some(VersionStage::Late) => 2,
        None => 3,
    };
    (rank, label.to_owned())
}

impl FixtureAdapter {
    /// Load and validate a fixture root.
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self> {
        use crate::error::HarnessError;
        let root = root.as_ref().to_path_buf();

        let kb_dir = root.join("kb");
        let entries = std::fs::read_dir(&kb_dir).map_err(|e| HarnessError::io(&kb_dir, e))?;
        let mut versions: Vec<(String, ToolKb)> = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| HarnessError::io(&kb_dir, e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(label) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let kb = ToolKb::read(&path)?;
            if kb.server_version != label {
                return Err(fixture_error(
                    "fixture KB",
                    format!(
                        "{}: server_version {:?} does not match the file name {label:?}",
                        path.display(),
                        kb.server_version
                    ),
                ));
            }
            versions.push((label, kb));
        }
        if versions.is_empty() {
            return Err(fixture_error(
                "fixture root",
                format!("no kb/<version>.json files in {}", root.display()),
            ));
        }
        versions.sort_by_key(|(label, _)| version_rank(label));

        let release_id = versions[0].1.release_id.clone();
        if let Some((label, kb)) = versions.iter().find(|(_, kb)| kb.release_id != release_id) {
            return Err(fixture_error(
                "fixture KB",
                format!(
                    "version {label:?} has release_id {:?}, expected {release_id:?}",
                    kb.release_id
                ),
            ));
        }

        let lineage_path = root.join("lineage.json");
        if lineage_path.exists() {
            let text = std::fs::read_to_string(&lineage_path)
                .map_err(|e| HarnessError::io(&lineage_path, e))?;
            let entries: Vec<LineageEntry> = serde_json::from_str(&text)?;
            apply_lineage(&mut versions, &entries)?;
        }

        let tasks_path = root.join("tasks.json");
        let text =
            std::fs::read_to_string(&tasks_path).map_err(|e| HarnessError::io(&tasks_path, e))?;
        let tasks: Vec<TaskSpec> = serde_json::from_str(&text)?;
        let mut seen = std::collections::BTreeSet::new();
        for task in &tasks {
            if !seen.insert(task.task_id.as_str()) {
                return Err(fixture_error(
                    "fixture tasks",
                    format!("duplicate task_id {:?}", task.task_id),
                ));
            }
            let known = versions
                .iter()
                .any(|(_, kb)| kb.cards.iter().any(|c| c.server_id == task.server_id));
            if !known {
                return Err(fixture_error(
                    "fixture tasks",
                    format!(
                        "task {:?} names unknown server {:?}",
                        task.task_id, task.server_id
                    ),
                ));
            }
        }

        Ok(Self {
            root,
            release_id,
            versions,
            tasks,
        })
    }

    /// Fixture root directory.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Version labels in order (`early`, `middle`, `late`, others).
    pub fn version_labels(&self) -> Vec<&str> {
        self.versions.iter().map(|(l, _)| l.as_str()).collect()
    }
}

/// Apply the explicit lineage map to the loaded KBs.
fn apply_lineage(versions: &mut [(String, ToolKb)], entries: &[LineageEntry]) -> Result<()> {
    let order: Vec<String> = versions.iter().map(|(l, _)| l.clone()).collect();
    for entry in entries {
        let Some(start) = order.iter().position(|l| *l == entry.from_version) else {
            return Err(fixture_error(
                "fixture lineage",
                format!("unknown from_version {:?}", entry.from_version),
            ));
        };
        let mut matched = false;
        for (_, kb) in &mut versions[start..] {
            for card in &mut kb.cards {
                if card.server_id == entry.server_id && card.tool_id == entry.tool_id {
                    card.lineage_id = entry.lineage_id.clone();
                    matched = true;
                }
            }
        }
        if !matched {
            return Err(fixture_error(
                "fixture lineage",
                format!(
                    "no card {:?}/{:?} at or after version {:?}",
                    entry.server_id, entry.tool_id, entry.from_version
                ),
            ));
        }
    }
    Ok(())
}

impl SourceAdapter for FixtureAdapter {
    fn release_id(&self) -> &str {
        &self.release_id
    }

    fn inventory(&self) -> Result<Inventory> {
        let mut servers: std::collections::BTreeMap<&str, Vec<VersionInfo>> =
            std::collections::BTreeMap::new();
        for (label, kb) in &self.versions {
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for card in &kb.cards {
                *counts.entry(card.server_id.as_str()).or_default() += 1;
            }
            for (server, tool_count) in counts {
                servers.entry(server).or_default().push(VersionInfo {
                    label: label.clone(),
                    stage: stage_of(label),
                    tool_count,
                });
            }
        }
        Ok(Inventory {
            release_id: self.release_id.clone(),
            servers: servers
                .into_iter()
                .map(|(server_id, versions)| ServerInfo {
                    server_id: server_id.to_owned(),
                    versions,
                })
                .collect(),
            tasks: self.tasks.clone(),
        })
    }

    fn version_labels(&self) -> Vec<String> {
        self.versions.iter().map(|(l, _)| l.clone()).collect()
    }

    fn load_kb(&self, version: &str) -> Result<ToolKb> {
        self.versions
            .iter()
            .find(|(label, _)| label == version)
            .map(|(_, kb)| kb.clone())
            .ok_or_else(|| {
                fixture_error(
                    "fixture version",
                    format!(
                        "unknown version {version:?} (available: {})",
                        FixtureAdapter::version_labels(self).join(", ")
                    ),
                )
            })
    }
}
