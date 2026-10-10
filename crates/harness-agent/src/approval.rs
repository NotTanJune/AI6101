//! Approval policy for tool calls.
//!
//! Every tool call belongs to an [`ApprovalClass`]. The policy decides per class whether the
//! call runs immediately ([`PolicyAction::Allow`]) or needs the user ([`PolicyAction::Ask`]).
//! Answering [`Decision::Session`] allows the class for the rest of the session.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Text returned to the model when the user denies a tool call.
pub const DENIED_TEXT: &str = "The user denied this tool call.";

/// Kind of side effect a tool call can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass {
    /// Reads files or lists directories.
    Read,
    /// Creates or changes files.
    Write,
    /// Runs a shell command.
    Shell,
    /// Calls an MCP tool.
    Mcp,
}

impl ApprovalClass {
    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Shell => "shell",
            Self::Mcp => "mcp",
        }
    }
}

/// What the policy does for a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Run without asking.
    Allow,
    /// Ask the user first.
    Ask,
}

/// The user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow this call only.
    Once,
    /// Allow this call and every later call of the same class in this session.
    Session,
    /// Refuse this call.
    Deny,
}

/// A pending approval shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalRequest {
    /// Request id, unique within the session.
    pub id: u64,
    /// Exposed tool name.
    pub tool: String,
    /// Class of the call.
    pub class: ApprovalClass,
    /// One-line description of the call.
    pub summary: String,
    /// Unified diff of the proposed change, for file writes and edits.
    pub diff: Option<String>,
}

/// Per-class defaults plus the classes the user allowed for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPolicy {
    defaults: BTreeMap<ApprovalClass, PolicyAction>,
    session_allowed: BTreeSet<ApprovalClass>,
}

impl Default for ApprovalPolicy {
    /// `Read` and `Mcp` run freely; `Write` and `Shell` ask.
    fn default() -> Self {
        Self {
            defaults: BTreeMap::from([
                (ApprovalClass::Read, PolicyAction::Allow),
                (ApprovalClass::Write, PolicyAction::Ask),
                (ApprovalClass::Shell, PolicyAction::Ask),
                (ApprovalClass::Mcp, PolicyAction::Allow),
            ]),
            session_allowed: BTreeSet::new(),
        }
    }
}

impl ApprovalPolicy {
    /// Policy that never asks (benchmark runs).
    pub fn allow_all() -> Self {
        let mut policy = Self::default();
        for action in policy.defaults.values_mut() {
            *action = PolicyAction::Allow;
        }
        policy
    }

    /// Change the default for one class.
    #[must_use]
    pub fn with_default(mut self, class: ApprovalClass, action: PolicyAction) -> Self {
        self.defaults.insert(class, action);
        self
    }

    /// Default action for a class, ignoring session grants.
    pub fn default_action(&self, class: ApprovalClass) -> PolicyAction {
        self.defaults
            .get(&class)
            .copied()
            .unwrap_or(PolicyAction::Ask)
    }

    /// Whether a call of this class must be shown to the user.
    pub fn requires_approval(&self, class: ApprovalClass) -> bool {
        self.default_action(class) == PolicyAction::Ask && !self.session_allowed.contains(&class)
    }

    /// Record the user's decision. Returns whether the call may run.
    pub fn record(&mut self, class: ApprovalClass, decision: Decision) -> bool {
        match decision {
            Decision::Once => true,
            Decision::Session => {
                self.session_allowed.insert(class);
                true
            }
            Decision::Deny => false,
        }
    }

    /// Classes the user allowed for this session.
    pub fn session_allowed(&self) -> &BTreeSet<ApprovalClass> {
        &self.session_allowed
    }
}

/// Unified diff between `old` and `new` (line based, three lines of context).
///
/// Large inputs fall back to showing the whole change as one removal and one addition.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    const CONTEXT: usize = 3;
    const MAX_CELLS: usize = 4_000_000;
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];
    if old_mid.is_empty() && new_mid.is_empty() {
        return String::new();
    }

    // Edit script as (tag, line) with tag ' ', '-' or '+'.
    let mut script: Vec<(char, &str)> = old_lines[..prefix].iter().map(|l| (' ', *l)).collect();
    if old_mid.len().saturating_mul(new_mid.len()) <= MAX_CELLS {
        script.extend(lcs_script(old_mid, new_mid));
    } else {
        script.extend(old_mid.iter().map(|l| ('-', *l)));
        script.extend(new_mid.iter().map(|l| ('+', *l)));
    }
    script.extend(
        old_lines[old_lines.len() - suffix..]
            .iter()
            .map(|l| (' ', *l)),
    );

    // Group into hunks around changes.
    let changed: Vec<usize> = script
        .iter()
        .enumerate()
        .filter(|(_, (tag, _))| *tag != ' ')
        .map(|(i, _)| i)
        .collect();
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut end = (changed[i] + CONTEXT + 1).min(script.len());
        let mut j = i + 1;
        while j < changed.len() && changed[j].saturating_sub(CONTEXT) <= end {
            end = (changed[j] + CONTEXT + 1).min(script.len());
            j += 1;
        }
        let (mut old_start, mut new_start) = (1, 1);
        for (tag, _) in &script[..start] {
            if *tag != '+' {
                old_start += 1;
            }
            if *tag != '-' {
                new_start += 1;
            }
        }
        let old_len = script[start..end].iter().filter(|(t, _)| *t != '+').count();
        let new_len = script[start..end].iter().filter(|(t, _)| *t != '-').count();
        // Unified diff convention: an empty side starts at the line before.
        let old_start = if old_len == 0 {
            old_start - 1
        } else {
            old_start
        };
        let new_start = if new_len == 0 {
            new_start - 1
        } else {
            new_start
        };
        out.push_str(&format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"
        ));
        for (tag, line) in &script[start..end] {
            out.push(*tag);
            out.push_str(line);
            out.push('\n');
        }
        i = j;
    }
    out
}

fn lcs_script<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(char, &'a str)> {
    let (n, m) = (old.len(), new.len());
    // table[i][j] = LCS length of old[i..] and new[j..].
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            out.push((' ', old[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(('-', old[i]));
            i += 1;
        } else {
            out.push(('+', new[j]));
            j += 1;
        }
    }
    out.extend(old[i..].iter().map(|l| ('-', *l)));
    out.extend(new[j..].iter().map(|l| ('+', *l)));
    out
}
