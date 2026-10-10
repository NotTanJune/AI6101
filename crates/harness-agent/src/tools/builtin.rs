//! Built-in coding tools: `read_file`, `list_dir`, `grep`, `write_file`, `edit_file`,
//! `run_shell`.
//!
//! Every path is relative to a canonicalised workspace root and is resolved by
//! [`BuiltinTools::resolve`], which rejects `..` and symlink escapes. Tool failures are
//! returned as [`ToolOutput`] with `is_error` set and fed back to the model; nothing panics.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::{BuiltinKind, ToolSource, ToolSpec};
use crate::approval::{ApprovalClass, unified_diff};

/// Largest file `read_file`, `edit_file` and `grep` will open.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Default number of lines `read_file` returns.
pub const DEFAULT_READ_LINES: usize = 2000;
/// Most entries `list_dir` returns.
pub const MAX_LIST_ENTRIES: usize = 1000;
/// Most matches `grep` returns.
pub const MAX_GREP_MATCHES: usize = 200;
/// Cap on captured stdout and on captured stderr, each, for `run_shell`.
pub const MAX_SHELL_STREAM_BYTES: usize = 30_000;
/// Default `run_shell` timeout.
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;
/// Largest `run_shell` timeout.
pub const MAX_SHELL_TIMEOUT_SECS: u64 = 600;
/// Longest line `grep` and `read_file` show before cutting it.
const MAX_LINE_CHARS: usize = 2000;

/// Result of running a built-in tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text fed back to the model.
    pub text: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

impl ToolOutput {
    /// Successful output.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// Failed output.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// What a write-class call would do, for the approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// One-line description.
    pub summary: String,
    /// Unified diff of the change, when it can be computed.
    pub diff: Option<String>,
}

/// Approval class of a built-in tool.
pub fn approval_class(kind: BuiltinKind) -> ApprovalClass {
    match kind {
        BuiltinKind::ReadFile | BuiltinKind::ListDir | BuiltinKind::Grep => ApprovalClass::Read,
        BuiltinKind::WriteFile | BuiltinKind::EditFile => ApprovalClass::Write,
        BuiltinKind::RunShell => ApprovalClass::Shell,
    }
}

/// Specs (name, description, JSON Schema) of every built-in tool.
pub fn builtin_specs() -> Vec<ToolSpec> {
    let spec = |kind: BuiltinKind, description: &str, parameters: Value| ToolSpec {
        exposed_name: kind.name().to_owned(),
        description: description.to_owned(),
        parameters,
        source: ToolSource::Builtin(kind),
    };
    vec![
        spec(
            BuiltinKind::ReadFile,
            "Read a text file in the workspace. Returns numbered lines. Use offset and limit to page through large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start at (default 1)."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum lines to return (default 2000)."}
                },
                "required": ["path"]
            }),
        ),
        spec(
            BuiltinKind::ListDir,
            "List a directory in the workspace. Directories end with '/'. With glob, search recursively and return matching paths.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the workspace root (default '.')."},
                    "glob": {"type": "string", "description": "Optional glob such as '*.rs' or 'src/**/*.ts'; makes the listing recursive."}
                }
            }),
        ),
        spec(
            BuiltinKind::Grep,
            "Search file contents with a regular expression. Respects .gitignore. Returns 'file:line: text' for up to 200 matches.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax)."},
                    "path": {"type": "string", "description": "File or directory to search (default '.')."},
                    "glob": {"type": "string", "description": "Only search files whose path matches this glob."}
                },
                "required": ["pattern"]
            }),
        ),
        spec(
            BuiltinKind::WriteFile,
            "Create a file or overwrite it with the given content. Parent directories are created. Needs user approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root."},
                    "content": {"type": "string", "description": "Full new file content."}
                },
                "required": ["path", "content"]
            }),
        ),
        spec(
            BuiltinKind::EditFile,
            "Replace an exact string in a file. Fails when old_string is missing, or occurs more than once without replace_all. Needs user approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the workspace root."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        spec(
            BuiltinKind::RunShell,
            "Run a shell command with `sh -c` in the workspace root. Output is truncated to 30000 bytes per stream. Needs user approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to run."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Timeout in seconds (default 120, max 600)."}
                },
                "required": ["command"]
            }),
        ),
    ]
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ListArgs {
    path: Option<String>,
    glob: Option<String>,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    timeout_secs: Option<u64>,
}

/// The built-in tools bound to a workspace root.
#[derive(Debug, Clone)]
pub struct BuiltinTools {
    root: PathBuf,
    scrub_env: BTreeSet<String>,
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, ToolOutput> {
    if let Value::String(raw) = args {
        return Err(ToolOutput::error(format!(
            "tool arguments were not valid JSON: {}",
            truncate_chars(raw, 200)
        )));
    }
    serde_json::from_value(args.clone())
        .map_err(|e| ToolOutput::error(format!("invalid tool arguments: {e}")))
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut cut: String = text.chars().take(max).collect();
        cut.push_str("...");
        cut
    }
}

impl BuiltinTools {
    /// Bind the tools to `root` (canonicalised). `scrub_env` names environment variables that
    /// `run_shell` removes from the child's environment (the model API key variables).
    pub fn new(
        root: impl AsRef<Path>,
        scrub_env: impl IntoIterator<Item = String>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root)?,
            scrub_env: scrub_env.into_iter().collect(),
        })
    }

    /// Also remove these environment variables from `run_shell` children.
    pub fn add_scrub_env(&mut self, names: impl IntoIterator<Item = String>) {
        self.scrub_env.extend(names);
    }

    /// Environment variables removed from `run_shell` children.
    pub fn scrub_env(&self) -> &BTreeSet<String> {
        &self.scrub_env
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path to an absolute path inside the workspace.
    ///
    /// `..` is resolved lexically first. An existing path is canonicalised and must stay under
    /// the root (so symlinks pointing outside are refused, including dangling ones). A new path
    /// has its nearest existing ancestor canonicalised and checked the same way.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        if path.contains('\0') {
            return Err("path contains a NUL byte".to_owned());
        }
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        let mut normal = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::ParentDir => {
                    normal.pop();
                }
                Component::CurDir => {}
                other => normal.push(other.as_os_str()),
            }
        }
        let outside = || format!("path {path:?} is outside the workspace");
        // Find the longest prefix that exists on disk (a symlink counts as existing).
        let mut existing = normal.clone();
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        while std::fs::symlink_metadata(&existing).is_err() {
            let Some(name) = existing.file_name().map(std::ffi::OsStr::to_owned) else {
                return Err(outside());
            };
            rest.push(name);
            if !existing.pop() {
                return Err(outside());
            }
        }
        let mut canonical = std::fs::canonicalize(&existing).map_err(|e| {
            format!("cannot resolve path {path:?} (dangling symlink or unreadable): {e}")
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(outside());
        }
        for name in rest.into_iter().rev() {
            canonical.push(name);
        }
        Ok(canonical)
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
    }

    /// Describe what a write-class call would do, for the approval prompt. Other tools get a
    /// summary without a diff. Never fails: a call that cannot be previewed gets a plain summary.
    pub fn preview(&self, kind: BuiltinKind, args: &Value) -> Preview {
        match kind {
            BuiltinKind::WriteFile => match parse_args::<WriteArgs>(args) {
                Ok(a) => {
                    let (old, existed) = self
                        .resolve(&a.path)
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .map_or((String::new(), false), |t| (t, true));
                    Preview {
                        summary: format!(
                            "{} {} ({} bytes)",
                            if existed { "Overwrite" } else { "Create" },
                            a.path,
                            a.content.len()
                        ),
                        diff: Some(unified_diff(&a.path, &old, &a.content)),
                    }
                }
                Err(e) => Preview {
                    summary: format!("write_file with unusable arguments: {}", e.text),
                    diff: None,
                },
            },
            BuiltinKind::EditFile => match parse_args::<EditArgs>(args) {
                Ok(a) => {
                    let diff = self
                        .resolve(&a.path)
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .and_then(|old| {
                            let new = if a.replace_all.unwrap_or(false) {
                                old.replace(&a.old_string, &a.new_string)
                            } else {
                                old.replacen(&a.old_string, &a.new_string, 1)
                            };
                            (new != old).then(|| unified_diff(&a.path, &old, &new))
                        });
                    Preview {
                        summary: format!("Edit {}", a.path),
                        diff,
                    }
                }
                Err(e) => Preview {
                    summary: format!("edit_file with unusable arguments: {}", e.text),
                    diff: None,
                },
            },
            BuiltinKind::RunShell => Preview {
                summary: match parse_args::<ShellArgs>(args) {
                    Ok(a) => format!("Run: {}", truncate_chars(&a.command, 300)),
                    Err(e) => format!("run_shell with unusable arguments: {}", e.text),
                },
                diff: None,
            },
            other => Preview {
                summary: format!(
                    "{} {}",
                    other.name(),
                    truncate_chars(&args.to_string(), 200)
                ),
                diff: None,
            },
        }
    }

    /// Run a built-in tool. Failures are returned as error output, never as panics.
    pub async fn run(
        &self,
        kind: BuiltinKind,
        args: &Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let this = self.clone();
        let args = args.clone();
        match kind {
            BuiltinKind::RunShell => self.run_shell(&args, cancel).await,
            other => {
                let work = tokio::task::spawn_blocking(move || this.run_blocking(other, &args));
                match work.await {
                    Ok(output) => output,
                    Err(e) => ToolOutput::error(format!("tool task failed: {e}")),
                }
            }
        }
    }

    fn run_blocking(&self, kind: BuiltinKind, args: &Value) -> ToolOutput {
        match kind {
            BuiltinKind::ReadFile => self.read_file(args),
            BuiltinKind::ListDir => self.list_dir(args),
            BuiltinKind::Grep => self.grep(args),
            BuiltinKind::WriteFile => self.write_file(args),
            BuiltinKind::EditFile => self.edit_file(args),
            BuiltinKind::RunShell => ToolOutput::error("run_shell is asynchronous"),
        }
    }

    fn read_file(&self, args: &Value) -> ToolOutput {
        let a: ReadArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let path = match self.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let text = match read_text(&path) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(format!("{}: {e}", a.path)),
        };
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let offset = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_READ_LINES).max(1);
        if total == 0 {
            return ToolOutput::ok("[empty file]");
        }
        if offset > total {
            return ToolOutput::error(format!(
                "offset {offset} is past the end of the file ({total} lines)"
            ));
        }
        let end = (offset - 1 + limit).min(total);
        let mut out = String::new();
        for (index, line) in lines[offset - 1..end].iter().enumerate() {
            let _ = writeln!(
                out,
                "{:>6}\t{}",
                offset + index,
                truncate_chars(line, MAX_LINE_CHARS)
            );
        }
        if end < total {
            let _ = write!(
                out,
                "[truncated: showing lines {offset}-{end} of {total}; call again with offset {}]",
                end + 1
            );
        }
        ToolOutput::ok(out)
    }

    fn list_dir(&self, args: &Value) -> ToolOutput {
        let a: ListArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let dir_arg = a.path.as_deref().unwrap_or(".");
        let dir = match self.resolve(dir_arg) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        if !dir.is_dir() {
            return ToolOutput::error(format!("{dir_arg}: not a directory"));
        }
        let matcher = match a.glob.as_deref().map(compile_glob).transpose() {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let mut entries: Vec<String> = Vec::new();
        if let Some(matcher) = &matcher {
            let walker = WalkBuilder::new(&dir)
                .hidden(false)
                .require_git(false)
                .filter_entry(|e| e.file_name() != ".git")
                .build();
            for entry in walker.flatten() {
                if entry.depth() == 0 {
                    continue;
                }
                let relative = entry.path().strip_prefix(&dir).unwrap_or(entry.path());
                if glob_matches(matcher, relative) {
                    let mut shown = relative.display().to_string();
                    if entry.file_type().is_some_and(|t| t.is_dir()) {
                        shown.push('/');
                    }
                    entries.push(shown);
                }
            }
        } else {
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("{dir_arg}: {e}")),
            };
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                entries.push(if is_dir { format!("{name}/") } else { name });
            }
        }
        entries.sort();
        let total = entries.len();
        entries.truncate(MAX_LIST_ENTRIES);
        let mut out = entries.join("\n");
        if total > MAX_LIST_ENTRIES {
            let _ = write!(
                out,
                "\n[truncated: showing {MAX_LIST_ENTRIES} of {total} entries]"
            );
        }
        if out.is_empty() {
            out = "[no entries]".to_owned();
        }
        ToolOutput::ok(out)
    }

    fn grep(&self, args: &Value) -> ToolOutput {
        let a: GrepArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let regex = match RegexBuilder::new(&a.pattern).size_limit(10 << 20).build() {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
        };
        let matcher = match a.glob.as_deref().map(compile_glob).transpose() {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let target_arg = a.path.as_deref().unwrap_or(".");
        let target = match self.resolve(target_arg) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let walker = WalkBuilder::new(&target)
            .hidden(false)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        let mut out = String::new();
        let mut matches = 0usize;
        let mut limited = false;
        'files: for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let shown = self.display(path);
            if let Some(matcher) = &matcher {
                let relative = path.strip_prefix(&target).unwrap_or(path);
                if !glob_matches(matcher, relative) && !glob_matches(matcher, Path::new(&shown)) {
                    continue;
                }
            }
            let Ok(text) = read_text(path) else {
                continue; // binary, too large or unreadable: skip silently
            };
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    if matches == MAX_GREP_MATCHES {
                        limited = true;
                        break 'files;
                    }
                    matches += 1;
                    let _ = writeln!(out, "{shown}:{}: {}", index + 1, truncate_chars(line, 500));
                }
            }
        }
        if limited {
            let _ = write!(out, "[truncated: more than {MAX_GREP_MATCHES} matches]");
        }
        if out.is_empty() {
            out = "[no matches]".to_owned();
        }
        ToolOutput::ok(out)
    }

    fn write_file(&self, args: &Value) -> ToolOutput {
        let a: WriteArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if a.content.len() as u64 > MAX_FILE_BYTES {
            return ToolOutput::error("content is larger than 10 MiB");
        }
        let path = match self.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        if path.is_dir() {
            return ToolOutput::error(format!("{}: is a directory", a.path));
        }
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::error(format!("cannot create parent directories: {e}"));
        }
        // The parent now exists: re-resolve so a symlink created on the way cannot escape.
        let path = match self.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        match std::fs::write(&path, a.content.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!("Wrote {} bytes to {}", a.content.len(), a.path)),
            Err(e) => ToolOutput::error(format!("{}: {e}", a.path)),
        }
    }

    fn edit_file(&self, args: &Value) -> ToolOutput {
        let a: EditArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if a.old_string.is_empty() {
            return ToolOutput::error("old_string must not be empty");
        }
        let path = match self.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let text = match read_text(&path) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(format!("{}: {e}", a.path)),
        };
        let count = text.matches(&a.old_string).count();
        let replace_all = a.replace_all.unwrap_or(false);
        if count == 0 {
            return ToolOutput::error(format!("old_string was not found in {}", a.path));
        }
        if count > 1 && !replace_all {
            return ToolOutput::error(format!(
                "old_string occurs {count} times in {}; add surrounding context to make it unique or set replace_all",
                a.path
            ));
        }
        let updated = if replace_all {
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        match std::fs::write(&path, updated.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!(
                "Replaced {} occurrence{} in {}",
                if replace_all { count } else { 1 },
                if replace_all && count != 1 { "s" } else { "" },
                a.path
            )),
            Err(e) => ToolOutput::error(format!("{}: {e}", a.path)),
        }
    }

    async fn run_shell(&self, args: &Value, cancel: &CancellationToken) -> ToolOutput {
        let a: ShellArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let timeout_secs = a
            .timeout_secs
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS)
            .clamp(1, MAX_SHELL_TIMEOUT_SECS);
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&a.command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        for name in &self.scrub_env {
            cmd.env_remove(name);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("cannot start shell: {e}")),
        };
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(read_capped(stdout));
        let err_task = tokio::spawn(read_capped(stderr));

        enum Ending {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        let ending = tokio::select! {
            status = child.wait() => Ending::Exited(status),
            () = tokio::time::sleep(Duration::from_secs(timeout_secs)) => Ending::TimedOut,
            () = cancel.cancelled() => Ending::Cancelled,
        };
        if !matches!(ending, Ending::Exited(_)) {
            kill_group(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        // Readers finish once the pipes close; do not wait forever on stray grandchildren.
        let grace = Duration::from_secs(2);
        let (out, out_dropped) = tokio::time::timeout(grace, out_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let (err, err_dropped) = tokio::time::timeout(grace, err_task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();

        let mut text = String::new();
        let is_error;
        match ending {
            Ending::Exited(Ok(status)) => {
                is_error = !status.success();
                match status.code() {
                    Some(code) => {
                        let _ = writeln!(text, "exit code: {code}");
                    }
                    None => text.push_str("exit code: terminated by signal\n"),
                }
            }
            Ending::Exited(Err(e)) => {
                is_error = true;
                let _ = writeln!(text, "failed to wait for the command: {e}");
            }
            Ending::TimedOut => {
                is_error = true;
                let _ = writeln!(
                    text,
                    "[command timed out after {timeout_secs}s and was killed]"
                );
            }
            Ending::Cancelled => {
                is_error = true;
                text.push_str("[command cancelled and killed]\n");
            }
        }
        append_stream(&mut text, "stdout", &out, out_dropped);
        append_stream(&mut text, "stderr", &err, err_dropped);
        ToolOutput {
            text: text.trim_end().to_owned(),
            is_error,
        }
    }
}

fn append_stream(text: &mut String, label: &str, bytes: &[u8], dropped: usize) {
    if bytes.is_empty() && dropped == 0 {
        return;
    }
    let _ = writeln!(text, "--- {label} ---");
    text.push_str(&String::from_utf8_lossy(bytes));
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if dropped > 0 {
        let _ = writeln!(text, "[truncated {dropped} bytes]");
    }
}

/// Read a stream to the end, keeping the first [`MAX_SHELL_STREAM_BYTES`] bytes and counting
/// the rest.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> (Vec<u8>, usize) {
    let Some(mut stream) = stream else {
        return (Vec::new(), 0);
    };
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_SHELL_STREAM_BYTES.saturating_sub(kept.len());
                let take = room.min(n);
                kept.extend_from_slice(&buf[..take]);
                dropped += n - take;
            }
        }
    }
    (kept, dropped)
}

/// Kill the whole process group of the shell (the shell is its own group leader).
async fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let _ = tokio::process::Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file is {} bytes, larger than the 10 MiB limit",
            meta.len()
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8 text".to_owned())
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, String> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("invalid glob {pattern:?}: {e}"))
}

/// Match a glob against a relative path, and against the bare file name when the glob has no
/// directory part (so `*.rs` finds `src/lib.rs`).
fn glob_matches(matcher: &GlobMatcher, relative: &Path) -> bool {
    matcher.is_match(relative)
        || relative
            .file_name()
            .is_some_and(|name| matcher.is_match(Path::new(name)))
}
