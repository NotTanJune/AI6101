//! Tool registry: what the model is offered and how each tool call is routed.
//!
//! MCP tools are exposed from the **Tool KB**, not from the live server: the model sees the KB
//! description and input schema even when the live tool differs. That is the point of the
//! experiment (a stale KB against an evolved server). Live tools only report drift.

pub mod builtin;

use std::collections::BTreeMap;

use harness_core::model::ToolKb;
use serde_json::Value;

use crate::mcp::LiveTool;
use crate::model::ToolSchema;

/// Longest exposed tool name in [`NamingMode::Namespaced`].
pub const MAX_EXPOSED_NAME_LEN: usize = 64;

/// A built-in coding tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinKind {
    /// `read_file`
    ReadFile,
    /// `list_dir`
    ListDir,
    /// `grep`
    Grep,
    /// `write_file`
    WriteFile,
    /// `edit_file`
    EditFile,
    /// `run_shell`
    RunShell,
}

impl BuiltinKind {
    /// Every built-in tool in registration order.
    pub const ALL: [BuiltinKind; 6] = [
        Self::ReadFile,
        Self::ListDir,
        Self::Grep,
        Self::WriteFile,
        Self::EditFile,
        Self::RunShell,
    ];

    /// Exposed tool name.
    pub fn name(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListDir => "list_dir",
            Self::Grep => "grep",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::RunShell => "run_shell",
        }
    }
}

/// Where a tool call is routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// Built-in tool.
    Builtin(BuiltinKind),
    /// MCP tool: server name and the tool name to call on the live server.
    Mcp {
        /// Server name.
        server: String,
        /// Live tool name.
        tool: String,
    },
}

/// One tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Name the model sees and calls.
    pub exposed_name: String,
    /// Description the model sees.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    /// Routing target.
    pub source: ToolSource,
}

/// How MCP tool names are exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamingMode {
    /// `mcp__<server>__<tool>` (interactive TUI).
    Namespaced,
    /// The tool name as-is (benchmark).
    Bare,
}

/// Tool registry failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolsError {
    /// Two tools would be exposed under the same name.
    #[error("tool name collision: {name:?} is already exposed")]
    NameCollision {
        /// The clashing exposed name.
        name: String,
    },
}

/// What `add_mcp_from_kb` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExposeReport {
    /// Exposed names added to the registry.
    pub exposed: Vec<String>,
    /// KB tools the live server does not list (calls will fail until the KB is updated).
    pub kb_only: Vec<String>,
    /// Live tools missing from the KB. They are not exposed.
    pub live_only: Vec<String>,
}

/// The set of tools offered to the model.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    mode: NamingMode,
    specs: Vec<ToolSpec>,
}

impl ToolRegistry {
    /// Empty registry.
    pub fn new(mode: NamingMode) -> Self {
        Self {
            mode,
            specs: Vec::new(),
        }
    }

    /// Naming mode.
    pub fn mode(&self) -> NamingMode {
        self.mode
    }

    /// Add one spec; the exposed name must be unused.
    pub fn add(&mut self, spec: ToolSpec) -> Result<(), ToolsError> {
        if self.get(&spec.exposed_name).is_some() {
            return Err(ToolsError::NameCollision {
                name: spec.exposed_name,
            });
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Add the built-in coding tools.
    pub fn add_builtins(&mut self) -> Result<(), ToolsError> {
        for spec in builtin::builtin_specs() {
            self.add(spec)?;
        }
        Ok(())
    }

    /// Expose the KB cards of `server`, using the **KB** description and input schema.
    ///
    /// Live tools never change what the model sees. A KB card whose tool the live server does
    /// not list is still exposed (and reported in [`ExposeReport::kb_only`]); a live tool with
    /// no KB card is not exposed (reported in [`ExposeReport::live_only`]). Nothing is added
    /// when any name collides.
    pub fn add_mcp_from_kb(
        &mut self,
        server: &str,
        kb: &ToolKb,
        live: &[LiveTool],
    ) -> Result<ExposeReport, ToolsError> {
        let mut report = ExposeReport::default();
        let mut staged: Vec<ToolSpec> = Vec::new();
        for card in kb.cards.iter().filter(|c| c.server_id == server) {
            let exposed_name = match self.mode {
                NamingMode::Namespaced => namespaced_name(server, &card.name),
                NamingMode::Bare => card.name.clone(),
            };
            if self.get(&exposed_name).is_some()
                || staged.iter().any(|s| s.exposed_name == exposed_name)
            {
                return Err(ToolsError::NameCollision { name: exposed_name });
            }
            if !live
                .iter()
                .any(|l| l.name == card.tool_id || l.name == card.name)
            {
                report.kb_only.push(exposed_name.clone());
            }
            report.exposed.push(exposed_name.clone());
            staged.push(ToolSpec {
                exposed_name,
                description: card.description.clone(),
                parameters: card.input_schema.clone(),
                source: ToolSource::Mcp {
                    server: server.to_owned(),
                    tool: live
                        .iter()
                        .find(|l| l.name == card.name)
                        .map_or_else(|| card.tool_id.clone(), |l| l.name.clone()),
                },
            });
        }
        for tool in live {
            let in_kb = kb
                .cards
                .iter()
                .any(|c| c.server_id == server && (c.tool_id == tool.name || c.name == tool.name));
            if !in_kb {
                report.live_only.push(tool.name.clone());
            }
        }
        self.specs.extend(staged);
        Ok(report)
    }

    /// Remove the built-in tools.
    pub fn remove_builtins(&mut self) {
        self.specs
            .retain(|s| !matches!(s.source, ToolSource::Builtin(_)));
    }

    /// Whether any built-in tool is registered.
    pub fn has_builtins(&self) -> bool {
        self.specs
            .iter()
            .any(|s| matches!(s.source, ToolSource::Builtin(_)))
    }

    /// Remove every tool of an MCP server (before re-adding it from a new KB).
    pub fn remove_mcp_server(&mut self, server: &str) {
        self.specs.retain(|s| match &s.source {
            ToolSource::Mcp { server: srv, .. } => srv != server,
            ToolSource::Builtin(_) => true,
        });
    }

    /// Spec for an exposed name.
    pub fn get(&self, exposed_name: &str) -> Option<&ToolSpec> {
        self.specs.iter().find(|s| s.exposed_name == exposed_name)
    }

    /// All specs in registration order.
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Whether no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Schemas for a [`crate::model::ModelRequest`].
    pub fn to_schemas(&self) -> Vec<ToolSchema> {
        self.specs
            .iter()
            .map(|s| ToolSchema {
                name: s.exposed_name.clone(),
                description: s.description.clone(),
                parameters: s.parameters.clone(),
            })
            .collect()
    }

    /// Exposed names grouped by MCP server (built-ins under the empty key).
    pub fn names_by_server(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for spec in &self.specs {
            let key = match &spec.source {
                ToolSource::Builtin(_) => String::new(),
                ToolSource::Mcp { server, .. } => server.clone(),
            };
            out.entry(key).or_default().push(spec.exposed_name.clone());
        }
        out
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// FNV-1a 64-bit hash, rendered as six hex digits. Stable across runs and Rust versions.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// `mcp__<server>__<tool>` with unsafe characters replaced by `_`, truncated to
/// [`MAX_EXPOSED_NAME_LEN`] characters with a 6-character hash suffix when longer.
pub fn namespaced_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_EXPOSED_NAME_LEN {
        return full;
    }
    let keep = MAX_EXPOSED_NAME_LEN - 6;
    // The name is ASCII after sanitising, so byte slicing is safe.
    format!("{}{}", &full[..keep], short_hash(&full))
}
