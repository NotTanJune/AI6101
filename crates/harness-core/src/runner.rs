//! Bridge to the benchmark runner (Person B's Python workstream).
//!
//! [`BenchmarkRunner`] runs one stage (one strategy) and streams the runner's `run-event/1`
//! JSONL events back through a callback. Two implementations exist:
//!
//! - [`FixtureRunner`] replays checked-in JSONL logs (offline tests and the smoke run);
//! - [`ProcessRunner`] launches the real runner with `std::process::Command` and an argv array
//!   (never a shell), tails the JSONL file the child writes and parses each new line while the
//!   child is still running.
//!
//! ## Proposed runner argv (PROPOSAL, awaiting Person B agreement)
//!
//! ```text
//! <executable> [fixed_args...] run --run-id <id> --tool-kb <path> --server-version <label>
//!   --task-id <id> (repeated) --provider <p> --model <m> [--temperature <f>] [--top-p <f>]
//!   [--max-output-tokens <n>] --seed <n> --judge-version <v> --output-jsonl <path>
//!   --remaining-budget-usd <f> [--mock]
//! ```
//!
//! [`build_argv`] produces everything from `run` onward; the process runner prepends the
//! configured executable and fixed arguments. Optional decoding flags are omitted when unset.
//! Environment: dataset roots and the provider key variable are passed by NAME through the
//! runner's env passthrough list; the child gets a cleared environment plus a small base
//! (`PATH`, `HOME`, locale, temp dirs, Python path variables) plus the named variables. Secret
//! values never appear in argv. Exit code 0 means ok, 2 means the stage budget was exhausted
//! (the partial log is valid), any other exit is a failure (the partial log is preserved).
//!
//! Every line is validated against the run-event schema and fails closed with an error naming
//! the 1-based line. Human-readable console output of the child is never parsed.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::config::{RunnerKind, RunnerSection, Strategy};
use crate::events::{EventBody, EventError, EventSource, RunEvent, RunStatus, parse_event_line};
use crate::redact::Redactor;

/// Exit code by which the runner reports that the stage budget was exhausted.
pub const EXIT_BUDGET_EXHAUSTED: i32 = 2;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// Everything the runner needs for one stage.
#[derive(Debug, Clone, PartialEq)]
pub struct StageRequest {
    /// Run id stamped on the stage's events.
    pub run_id: String,
    /// Strategy being evaluated (selects the fixture; not passed in argv).
    pub strategy: Strategy,
    /// Path of the Tool KB JSON the agent must use.
    pub tool_kb_path: PathBuf,
    /// Server version label to evaluate against.
    pub server_version: String,
    /// Fixed task ids.
    pub task_ids: Vec<String>,
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling value.
    pub top_p: Option<f64>,
    /// Output token limit.
    pub max_output_tokens: Option<u32>,
    /// Experiment seed.
    pub seed: u64,
    /// Judge version.
    pub judge_version: String,
    /// File the runner writes its JSONL events to.
    pub output_jsonl: PathBuf,
    /// Remaining local dispatch budget for this stage.
    pub remaining_budget_usd: f64,
    /// Ask the runner for its offline mock mode (`--mock`).
    pub mock: bool,
}

/// How the runner finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code; `None` when the child was terminated by a signal.
    pub code: Option<i32>,
}

impl ExitInfo {
    /// Exit code 0.
    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Result of a finished stage (exit 0 or budget-exhausted exit 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOutcome {
    /// Exit information.
    pub exit: ExitInfo,
    /// Number of valid events delivered to the callback.
    pub events: usize,
    /// Raw JSONL log kept in the run output.
    pub raw_path: PathBuf,
    /// The stage stopped because its budget was exhausted (exit 2 or a
    /// `run_complete` event with status `budget_exhausted`).
    pub budget_exhausted: bool,
    /// The raw log contained secret-looking text that was scrubbed after the fact.
    pub raw_redacted: bool,
}

/// Failure while running a stage.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// Filesystem failure.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The replay log for the strategy does not exist.
    #[error("no fixture log at {}", path.display())]
    MissingFixture {
        /// Expected fixture path.
        path: PathBuf,
    },
    /// A line failed validation (malformed JSON, schema violation, unsupported version).
    #[error("invalid runner event in {}: {source} ({events_before} valid event(s) delivered before it)", raw_path.display())]
    InvalidEvent {
        /// Underlying error; names the 1-based line.
        #[source]
        source: EventError,
        /// Valid events delivered before the failing line.
        events_before: usize,
        /// Raw log that was kept.
        raw_path: PathBuf,
    },
    /// A valid event violates the runner contract (wrong source or run id).
    #[error("runner contract violation at line {line} in {}: {message}", raw_path.display())]
    Contract {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
        /// Valid events delivered before the failing line.
        events_before: usize,
        /// Raw log that was kept.
        raw_path: PathBuf,
    },
    /// The child could not be started.
    #[error("cannot start runner {executable:?}: {source}")]
    Spawn {
        /// Executable that failed to start.
        executable: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The child exited with a failure code (or was killed). Partial events were delivered and
    /// the raw log is preserved.
    #[error(
        "runner exited with {} after {events} event(s); partial log kept at {}{}",
        display_exit(exit), raw_path.display(), display_tail(stderr_tail)
    )]
    ChildExit {
        /// Exit information.
        exit: ExitInfo,
        /// Valid events delivered before the exit.
        events: usize,
        /// Raw log that was kept.
        raw_path: PathBuf,
        /// Last part of the child's stderr (redacted).
        stderr_tail: String,
    },
    /// The stage was cancelled; partial events were delivered.
    #[error("stage cancelled after {events} event(s); partial log kept at {}", raw_path.display())]
    Cancelled {
        /// Valid events delivered before cancellation.
        events: usize,
        /// Raw log that was kept.
        raw_path: PathBuf,
    },
    /// The request or runner configuration is unusable.
    #[error("invalid runner request: {0}")]
    InvalidRequest(String),
}

fn display_exit(exit: &ExitInfo) -> String {
    match exit.code {
        Some(code) => format!("exit code {code}"),
        None => "no exit code (terminated by signal)".to_owned(),
    }
}

fn display_tail(tail: &str) -> String {
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!("; stderr: {}", tail.trim())
    }
}

impl RunnerError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Runs one benchmark stage and streams its events.
pub trait BenchmarkRunner: Send {
    /// Run the stage described by `req`. Every valid event is passed to `on_event` as soon as
    /// it is read. Exit 0 and budget-exhausted (exit 2) return `Ok`; other failures return
    /// `Err` after any partial events were delivered and the raw log was preserved.
    fn run_stage(
        &mut self,
        req: &StageRequest,
        on_event: &mut dyn FnMut(RunEvent),
    ) -> Result<StageOutcome, RunnerError>;
}

// ---------------------------------------------------------------------------------------------
// Argument builder
// ---------------------------------------------------------------------------------------------

/// Build the runner argv from `run` onward (the proposed contract, see the module docs).
/// Pure: no shell quoting, one `OsString` per argument, secrets are never part of a request.
pub fn build_argv(req: &StageRequest) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![OsString::from("run")];
    let mut flag = |name: &str, value: OsString| {
        argv.push(OsString::from(name));
        argv.push(value);
    };
    flag("--run-id", OsString::from(&req.run_id));
    flag("--tool-kb", req.tool_kb_path.clone().into_os_string());
    flag("--server-version", OsString::from(&req.server_version));
    for task in &req.task_ids {
        flag("--task-id", OsString::from(task));
    }
    flag("--provider", OsString::from(&req.provider));
    flag("--model", OsString::from(&req.model));
    if let Some(t) = req.temperature {
        flag("--temperature", OsString::from(t.to_string()));
    }
    if let Some(p) = req.top_p {
        flag("--top-p", OsString::from(p.to_string()));
    }
    if let Some(n) = req.max_output_tokens {
        flag("--max-output-tokens", OsString::from(n.to_string()));
    }
    flag("--seed", OsString::from(req.seed.to_string()));
    flag("--judge-version", OsString::from(&req.judge_version));
    flag("--output-jsonl", req.output_jsonl.clone().into_os_string());
    flag(
        "--remaining-budget-usd",
        OsString::from(req.remaining_budget_usd.to_string()),
    );
    if req.mock {
        argv.push(OsString::from("--mock"));
    }
    argv
}

// ---------------------------------------------------------------------------------------------
// Shared line handling
// ---------------------------------------------------------------------------------------------

enum LineError {
    Event(EventError),
    Contract { line: usize, message: String },
}

impl LineError {
    fn into_runner_error(self, events_before: usize, raw_path: &Path) -> RunnerError {
        match self {
            Self::Event(source) => RunnerError::InvalidEvent {
                source,
                events_before,
                raw_path: raw_path.to_owned(),
            },
            Self::Contract { line, message } => RunnerError::Contract {
                line,
                message,
                events_before,
                raw_path: raw_path.to_owned(),
            },
        }
    }
}

/// Parses, checks and forwards lines; counts events.
struct LineHandler<'a> {
    run_id: &'a str,
    strategy: Strategy,
    /// Fixture logs carry their own run id: rewrite it. Real runners must echo ours.
    rewrite_run_id: bool,
    redactor: &'a Redactor,
    events: usize,
    budget_exhausted: bool,
    redacted_any: bool,
}

impl<'a> LineHandler<'a> {
    fn new(req: &'a StageRequest, rewrite_run_id: bool, redactor: &'a Redactor) -> LineHandler<'a> {
        LineHandler {
            run_id: &req.run_id,
            strategy: req.strategy,
            rewrite_run_id,
            redactor,
            events: 0,
            budget_exhausted: false,
            redacted_any: false,
        }
    }

    /// Redact, parse and validate one raw line, then hand the event to `on_event`.
    /// Returns the redacted line text (what should be kept in the raw log).
    fn handle(
        &mut self,
        line_no: usize,
        raw: &str,
        on_event: &mut dyn FnMut(RunEvent),
    ) -> Result<String, LineError> {
        let redacted = self.redactor.redact_str(raw);
        if redacted != raw {
            self.redacted_any = true;
        }
        let mut event = parse_event_line(line_no, &redacted).map_err(LineError::Event)?;
        if event.source != EventSource::Python {
            return Err(LineError::Contract {
                line: line_no,
                message: "runner events must have source \"python\"".to_owned(),
            });
        }
        if self.rewrite_run_id {
            event.run_id = self.run_id.to_owned();
        } else if event.run_id != self.run_id {
            return Err(LineError::Contract {
                line: line_no,
                message: format!(
                    "run_id {:?} does not match the requested run id {:?}",
                    event.run_id, self.run_id
                ),
            });
        }
        if event.strategy.is_none() {
            event.strategy = Some(self.strategy);
        }
        if let EventBody::RunComplete(done) = &event.body
            && done.status == RunStatus::BudgetExhausted
        {
            self.budget_exhausted = true;
        }
        self.events += 1;
        on_event(event);
        Ok(redacted)
    }
}

fn prepare_output(path: &Path) -> Result<(), RunnerError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| RunnerError::io(parent, e))?;
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RunnerError::io(path, e)),
    }
}

// ---------------------------------------------------------------------------------------------
// FixtureRunner
// ---------------------------------------------------------------------------------------------

/// Replays `<fixture_dir>/<strategy>.jsonl`.
///
/// Each line is validated with [`parse_event_line`]; the event's `run_id` is rewritten to the
/// requested one. Lines are kept in `output_jsonl` as read (after secret redaction), so the raw
/// log preserves the fixture verbatim, original run id included.
#[derive(Debug, Clone)]
pub struct FixtureRunner {
    dir: PathBuf,
    redactor: Redactor,
    simulate_exit: Option<i32>,
    max_events: Option<usize>,
}

impl FixtureRunner {
    /// Replay logs from `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            redactor: Redactor::default(),
            simulate_exit: None,
            max_events: None,
        }
    }

    /// Redact these secrets from replayed lines.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Pretend the child exited with `code` after the replay (0, 2 or a failure code).
    #[must_use]
    pub fn with_simulated_exit(mut self, code: i32) -> Self {
        self.simulate_exit = Some(code);
        self
    }

    /// Replay only the first `n` lines, simulating a partial log.
    #[must_use]
    pub fn with_max_events(mut self, n: usize) -> Self {
        self.max_events = Some(n);
        self
    }

    /// Path of the fixture for `strategy`.
    pub fn fixture_path(&self, strategy: Strategy) -> PathBuf {
        self.dir.join(format!("{}.jsonl", strategy.as_str()))
    }
}

impl BenchmarkRunner for FixtureRunner {
    fn run_stage(
        &mut self,
        req: &StageRequest,
        on_event: &mut dyn FnMut(RunEvent),
    ) -> Result<StageOutcome, RunnerError> {
        let path = self.fixture_path(req.strategy);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(RunnerError::MissingFixture { path });
            }
            Err(e) => return Err(RunnerError::io(path, e)),
        };
        prepare_output(&req.output_jsonl)?;
        let mut raw =
            File::create(&req.output_jsonl).map_err(|e| RunnerError::io(&req.output_jsonl, e))?;

        let mut handler = LineHandler::new(req, true, &self.redactor);
        for (i, line) in text.lines().enumerate() {
            if self.max_events.is_some_and(|max| i >= max) {
                break;
            }
            let outcome = handler.handle(i + 1, line, on_event);
            // Keep the line that was read, valid or not, so the raw log shows what was seen.
            let kept = outcome.as_deref().unwrap_or(line);
            let kept = if outcome.is_err() {
                self.redactor.redact_str(kept)
            } else {
                kept.to_owned()
            };
            writeln!(raw, "{kept}").map_err(|e| RunnerError::io(&req.output_jsonl, e))?;
            if let Err(e) = outcome {
                return Err(e.into_runner_error(handler.events, &req.output_jsonl));
            }
        }
        raw.flush()
            .map_err(|e| RunnerError::io(&req.output_jsonl, e))?;

        let exit = ExitInfo {
            code: Some(self.simulate_exit.unwrap_or(0)),
        };
        finish(
            exit,
            &handler,
            &req.output_jsonl,
            String::from("simulated child exit"),
        )
    }
}

/// Map exit info to the stage result.
fn finish(
    exit: ExitInfo,
    handler: &LineHandler<'_>,
    raw_path: &Path,
    stderr_tail: String,
) -> Result<StageOutcome, RunnerError> {
    match exit.code {
        Some(0) | Some(EXIT_BUDGET_EXHAUSTED) => Ok(StageOutcome {
            exit,
            events: handler.events,
            raw_path: raw_path.to_owned(),
            budget_exhausted: exit.code == Some(EXIT_BUDGET_EXHAUSTED) || handler.budget_exhausted,
            raw_redacted: handler.redacted_any,
        }),
        _ => Err(RunnerError::ChildExit {
            exit,
            events: handler.events,
            raw_path: raw_path.to_owned(),
            stderr_tail,
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// ProcessRunner
// ---------------------------------------------------------------------------------------------

/// Environment variables inherited by the child in addition to the named passthrough ones.
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "VIRTUAL_ENV",
    "PYTHONHOME",
    "PYTHONPATH",
];

const STDERR_TAIL_BYTES: usize = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send>;

/// Launches the real benchmark runner as a child process.
///
/// No shell is involved: the executable, the configured fixed arguments and [`build_argv`]
/// output are passed as an argv array. The child environment is cleared and rebuilt from a small
/// base plus the named passthrough variables, so credentials reach the child only through
/// variables that were explicitly named. The JSONL file the child writes is tailed while it
/// runs; every complete line is validated and forwarded immediately.
pub struct ProcessRunner {
    executable: String,
    fixed_args: Vec<String>,
    passthrough_env: Vec<String>,
    extra_env: Vec<(String, String)>,
    redactor: Redactor,
    lookup: EnvLookup,
    cancel: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for ProcessRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRunner")
            .field("executable", &self.executable)
            .field("fixed_args", &self.fixed_args)
            .field("passthrough_env", &self.passthrough_env)
            .finish_non_exhaustive()
    }
}

/// Whether an env var NAME looks like it holds a credential.
fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

impl ProcessRunner {
    /// Runner for `executable` with `fixed_args` prepended to the contract arguments.
    pub fn new(executable: impl Into<String>, fixed_args: Vec<String>) -> Self {
        Self {
            executable: executable.into(),
            fixed_args,
            passthrough_env: Vec::new(),
            extra_env: Vec::new(),
            redactor: Redactor::default(),
            lookup: Box::new(|name| std::env::var(name).ok()),
            cancel: None,
        }
    }

    /// Names of environment variables passed to the child (dataset roots, provider key).
    /// Values are read when the stage starts; variables whose name looks like a credential are
    /// also registered for redaction.
    #[must_use]
    pub fn with_passthrough_env<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.passthrough_env = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set a non-secret variable for the child (for example a data directory).
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((name.into(), value.into()));
        self
    }

    /// Additional redaction rules applied to stderr, events and the raw log.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Replace the passthrough lookup (default: the process environment). For tests.
    #[must_use]
    pub fn with_env_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String> + Send + 'static,
    ) -> Self {
        self.lookup = Box::new(lookup);
        self
    }

    /// Stop the stage (kill the child) when `flag` becomes true.
    #[must_use]
    pub fn with_cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// Full argv (excluding the executable) for `req`: fixed args then [`build_argv`].
    pub fn full_args(&self, req: &StageRequest) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.fixed_args.iter().map(OsString::from).collect();
        args.extend(build_argv(req));
        args
    }

    fn effective_redactor(&self) -> Redactor {
        let mut redactor = self.redactor.clone();
        for name in self
            .passthrough_env
            .iter()
            .filter(|n| is_secret_env_name(n))
        {
            if let Some(value) = (self.lookup)(name) {
                redactor.add_secret(value);
            }
        }
        redactor
    }

    fn validate(&self, req: &StageRequest, redactor: &Redactor) -> Result<(), RunnerError> {
        if !req.remaining_budget_usd.is_finite() || req.remaining_budget_usd < 0.0 {
            return Err(RunnerError::InvalidRequest(
                "remaining budget must be a finite, non-negative number".to_owned(),
            ));
        }
        if self.executable.trim().is_empty() {
            return Err(RunnerError::InvalidRequest(
                "runner executable is empty".to_owned(),
            ));
        }
        // Defense in depth: a credential must never travel in argv.
        for (i, arg) in self.full_args(req).iter().enumerate() {
            if redactor.looks_like_secret(&arg.to_string_lossy()) {
                return Err(RunnerError::InvalidRequest(format!(
                    "argument {i} looks like a credential; secrets must be passed by environment variable"
                )));
            }
        }
        Ok(())
    }

    fn command(&self, req: &StageRequest) -> Command {
        let mut cmd = Command::new(&self.executable);
        cmd.args(self.full_args(req))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_clear();
        for name in BASE_ENV {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        for name in &self.passthrough_env {
            if let Some(value) = (self.lookup)(name) {
                cmd.env(name, value);
            }
        }
        for (name, value) in &self.extra_env {
            cmd.env(name, value);
        }
        cmd
    }

    fn spawn(&self, req: &StageRequest) -> Result<std::process::Child, RunnerError> {
        let mut attempts = 0;
        loop {
            match self.command(req).spawn() {
                Ok(child) => return Ok(child),
                // A freshly written executable can be briefly busy on Unix.
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 20 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(25));
                }
                Err(source) => {
                    return Err(RunnerError::Spawn {
                        executable: self.executable.clone(),
                        source,
                    });
                }
            }
        }
    }
}

/// Incremental reader of the child's JSONL file.
struct Tail {
    path: PathBuf,
    file: Option<File>,
    pending: Vec<u8>,
    next_line: usize,
}

impl Tail {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            file: None,
            pending: Vec::new(),
            next_line: 1,
        }
    }

    /// Read whatever the child appended since the last call.
    fn read_new(&mut self) -> Result<(), RunnerError> {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(f) => self.file = Some(f),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(RunnerError::io(&self.path, e)),
            }
        }
        if let Some(file) = self.file.as_mut() {
            file.read_to_end(&mut self.pending)
                .map_err(|e| RunnerError::io(&self.path, e))?;
        }
        Ok(())
    }

    /// Next complete (newline-terminated) line, without the terminator.
    fn next_complete(&mut self) -> Option<(usize, Vec<u8>)> {
        let pos = self.pending.iter().position(|b| *b == b'\n')?;
        let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let no = self.next_line;
        self.next_line += 1;
        Some((no, line))
    }

    /// Unterminated trailing text, if any.
    fn take_remainder(&mut self) -> Option<(usize, Vec<u8>)> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        let no = self.next_line;
        self.next_line += 1;
        Some((no, line))
    }
}

fn line_text(no: usize, bytes: Vec<u8>) -> Result<String, LineError> {
    String::from_utf8(bytes).map_err(|_| {
        LineError::Event(EventError::BadJson {
            line: no,
            message: "line is not valid UTF-8".to_owned(),
        })
    })
}

/// Drain complete lines through the handler.
fn pump_lines(
    tail: &mut Tail,
    handler: &mut LineHandler<'_>,
    on_event: &mut dyn FnMut(RunEvent),
) -> Result<(), LineError> {
    while let Some((no, bytes)) = tail.next_complete() {
        let text = line_text(no, bytes)?;
        handler.handle(no, &text, on_event)?;
    }
    Ok(())
}

/// Rewrite the raw log with secrets scrubbed, only when something had to be scrubbed.
fn scrub_raw(path: &Path, redactor: &Redactor) -> io::Result<bool> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&bytes);
    let scrubbed: String = text
        .split_inclusive('\n')
        .map(|line| redactor.redact_str(line))
        .collect();
    if scrubbed.as_bytes() == bytes.as_slice() {
        return Ok(false);
    }
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(scrubbed.as_bytes())?;
    file.flush()?;
    Ok(true)
}

fn drain_stderr(mut stderr: std::process::ChildStderr) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let cut = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..cut);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    })
}

impl BenchmarkRunner for ProcessRunner {
    fn run_stage(
        &mut self,
        req: &StageRequest,
        on_event: &mut dyn FnMut(RunEvent),
    ) -> Result<StageOutcome, RunnerError> {
        let redactor = self.effective_redactor();
        self.validate(req, &redactor)?;
        prepare_output(&req.output_jsonl)?;

        let mut child = self.spawn(req)?;
        let stderr_reader = child.stderr.take().map(drain_stderr);
        let mut tail = Tail::new(&req.output_jsonl);
        let mut handler = LineHandler::new(req, false, &redactor);
        let raw_path = req.output_jsonl.clone();

        // Poll: read new lines while the child runs, then drain once more after it exits.
        let status = loop {
            tail.read_new()?;
            if let Err(e) = pump_lines(&mut tail, &mut handler, on_event) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = scrub_raw(&raw_path, &redactor);
                return Err(e.into_runner_error(handler.events, &raw_path));
            }
            if self
                .cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                let _ = child.kill();
                let _ = child.wait();
                let _ = scrub_raw(&raw_path, &redactor);
                return Err(RunnerError::Cancelled {
                    events: handler.events,
                    raw_path,
                });
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunnerError::io(&self.executable, e));
                }
            }
        };

        let stderr_tail = stderr_reader
            .and_then(|h| h.join().ok())
            .map(|s| redactor.redact_str(&s))
            .unwrap_or_default();
        let exit = ExitInfo {
            code: status.code(),
        };

        tail.read_new()?;
        let mut result = pump_lines(&mut tail, &mut handler, on_event);
        if result.is_ok()
            && let Some((no, bytes)) = tail.take_remainder()
        {
            let outcome = line_text(no, bytes).and_then(|t| handler.handle(no, &t, on_event));
            // A failing child may have died mid-line: only a clean exit must fail closed on it.
            if outcome.is_err() && matches!(exit.code, Some(0) | Some(EXIT_BUDGET_EXHAUSTED)) {
                result = outcome.map(|_| ());
            }
        }
        let scrubbed = scrub_raw(&raw_path, &redactor).unwrap_or(false);
        if let Err(e) = result {
            return Err(e.into_runner_error(handler.events, &raw_path));
        }
        handler.redacted_any |= scrubbed;
        finish(exit, &handler, &raw_path, stderr_tail)
    }
}

// ---------------------------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------------------------

/// Build the runner selected by the `[runner]` config section.
///
/// `passthrough_env` lists environment variable NAMES forwarded to a process runner (dataset
/// roots such as `MCP_DATA_DIR` and `EVAL_DATA_DIR`, and the provider key variable). It is
/// ignored by the fixture runner.
pub fn build_runner(
    section: &RunnerSection,
    passthrough_env: &[String],
) -> Result<Box<dyn BenchmarkRunner>, RunnerError> {
    match section.kind {
        RunnerKind::Fixture => {
            let dir = section.fixture_dir.clone().ok_or_else(|| {
                RunnerError::InvalidRequest(
                    "runner.kind = \"fixture\" needs fixture_dir".to_owned(),
                )
            })?;
            Ok(Box::new(FixtureRunner::new(dir)))
        }
        RunnerKind::Process => {
            let executable = section
                .executable
                .clone()
                .filter(|e| !e.trim().is_empty())
                .ok_or_else(|| {
                    RunnerError::InvalidRequest(
                        "runner.kind = \"process\" needs executable".to_owned(),
                    )
                })?;
            Ok(Box::new(
                ProcessRunner::new(executable, section.fixed_args.clone())
                    .with_passthrough_env(passthrough_env.iter().cloned()),
            ))
        }
    }
}
