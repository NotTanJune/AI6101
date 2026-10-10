//! Implementations of the `validate`, `run` and `tui` commands.
//!
//! Each command returns a process exit code and writes to the given writers, so tests can call
//! them without spawning the binary. `validate` and `run` contain no pipeline logic: they call
//! `validate_config_inputs` and `run_matrix`. `tui` only wires the agent crate to the TUI crate.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::launcher::THEME_ENV;
use harness_agent::agent::{Agent, AgentConfig, AgentParts};
use harness_agent::approval::ApprovalPolicy;
use harness_agent::drift::SessionKb;
use harness_agent::mcp::McpConfig;
use harness_agent::model::ModelClient;
use harness_agent::model::gemini::GeminiClient;
use harness_agent::model::mock::MockModelClient;
use harness_agent::model::openai::OpenAiClient;
use harness_agent::pricing::rate_card;
use harness_agent::registry::{Effort, ModelRegistry, ProviderId};
use harness_agent::tools::builtin::BuiltinTools;
use harness_agent::tools::{NamingMode, ToolRegistry};
use harness_agent::transcript::TranscriptWriter;
use harness_core::cost::CostLedger;
use harness_core::model::{TOOL_KB_SCHEMA_VERSION, ToolKb};
use harness_core::redact::Redactor;
use harness_tui::theme::config::{
    ConfigSource, ThemeChoice, ThemeSpec, check_json as check_theme_json, load as load_theme_config,
};
use harness_tui::{
    AgentHandle, AgentSetup, App, AppInit, ConfigLauncher, McpLauncher, ModelInfo, NoMcp,
};

use harness_core::pipeline::{RunOptions, run_matrix, validate_config_inputs};
use harness_core::{Config, ConfigError, HarnessError, RunStatus, SystemClock};

/// Success.
pub const EXIT_OK: i32 = 0;
/// Configuration, input or setup error (also invalid configuration in `validate`).
pub const EXIT_ERROR: i32 = 1;
/// The run finished with status `failed`.
pub const EXIT_FAILED: i32 = 3;
/// The run finished with status `cancelled`.
pub const EXIT_CANCELLED: i32 = 4;
/// The run stopped because the local dispatch budget was exhausted.
pub const EXIT_BUDGET_EXHAUSTED: i32 = 5;

fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn load_config(path: &Path, err: &mut dyn Write) -> Option<Config> {
    match Config::load(path) {
        Ok(config) => Some(config),
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            None
        }
    }
}

fn print_error(error: &HarnessError, err: &mut dyn Write) {
    match error {
        HarnessError::Config(ConfigError::Invalid { problems }) => {
            let _ = writeln!(err, "error: {} problem(s) found:", problems.len());
            for problem in problems {
                let _ = writeln!(err, "  - {problem}");
            }
        }
        other => {
            let _ = writeln!(err, "error: {other}");
        }
    }
}

/// `harness validate --config <path>`: exit 0 with a readable report, or nonzero listing every
/// problem. Never contacts a provider or the network.
pub fn validate(config_path: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(config) = load_config(config_path, err) else {
        return EXIT_ERROR;
    };
    match validate_config_inputs(&config, &real_env) {
        Ok(report) => {
            let _ = writeln!(out, "{report}");
            EXIT_OK
        }
        Err(e) => {
            print_error(&e, err);
            EXIT_ERROR
        }
    }
}

/// `harness run --config <path> --output <dir>`: run the matrix headlessly and print a
/// per-strategy summary.
pub fn run(
    config_path: &Path,
    output_dir: &Path,
    force: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let Some(config) = load_config(config_path, err) else {
        return EXIT_ERROR;
    };
    let opts = RunOptions::new(output_dir)
        .with_clock(Arc::new(SystemClock))
        .with_overwrite(force)
        .with_progress(Box::new(|strategy, text| {
            // Live progress goes straight to the terminal's stderr.
            eprintln!("[{}] {text}", strategy.map_or("run", |s| s.as_str()));
        }));
    let result = run_matrix(&config, opts);
    match result {
        Ok(summary) => {
            let _ = writeln!(out, "{summary}");
            let _ = writeln!(out, "output: {}", output_dir.display());
            let _ = writeln!(
                out,
                "files: manifest.toml, events.jsonl, summary.json, kb/, python/"
            );
            match summary.status {
                RunStatus::Ok => EXIT_OK,
                RunStatus::Failed => EXIT_FAILED,
                RunStatus::Cancelled => EXIT_CANCELLED,
                RunStatus::BudgetExhausted => EXIT_BUDGET_EXHAUSTED,
            }
        }
        Err(e) => {
            print_error(&e, err);
            EXIT_ERROR
        }
    }
}

/// Default model alias of `harness tui`.
pub const DEFAULT_TUI_MODEL: &str = "luna";

/// Default per-session budget of `harness tui`, in USD.
pub const DEFAULT_TUI_BUDGET_USD: f64 = 5.0;

/// The model registry compiled into the binary (`configs/models.toml`). Used when no
/// `--models` file is given, so the TUI starts from any directory.
pub const EMBEDDED_MODELS_TOML: &str = include_str!("../../../configs/models.toml");

/// Load the registry from `path`, or the built-in one when `path` is `None`.
pub fn load_registry(path: Option<&Path>) -> Result<ModelRegistry, String> {
    match path {
        Some(path) => ModelRegistry::load(path).map_err(|e| e.to_string()),
        None => ModelRegistry::from_toml_str(EMBEDDED_MODELS_TOML)
            .map_err(|e| format!("the built-in model registry is invalid: {e}")),
    }
}

/// System instructions of the coding agent.
const AGENT_INSTRUCTIONS: &str = "You are a coding agent working in the user's workspace through a terminal. \
Use the built-in tools (read_file, list_dir, grep, write_file, edit_file, run_shell) to inspect and change files \
inside the workspace; the user approves writes and shell commands. MCP tools, when present, are named \
mcp__<server>__<tool>. Be concise, say what you changed, and ask when a request is ambiguous.";

/// Options of `harness tui`.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// Workspace root (current directory when `None`).
    pub workspace: Option<PathBuf>,
    /// Starting model alias.
    pub model: String,
    /// Starting effort (the model's default when `None`).
    pub effort: Option<String>,
    /// Model registry file; the registry built into the binary when `None`.
    pub models: Option<PathBuf>,
    /// `mcp.json` file; no servers when `None`.
    pub mcp_config: Option<PathBuf>,
    /// Tool KB file; no KB (every server unmanaged) when `None`.
    pub kb: Option<PathBuf>,
    /// Directory for session transcripts (`<workspace>/.harness/sessions` when `None`).
    pub sessions_dir: Option<PathBuf>,
    /// Budget cap for the session in USD.
    pub budget_usd: f64,
    /// Scripted model replies (a directory of numbered JSON files); no network when set.
    pub mock_script: Option<PathBuf>,
    /// Replaces the missing-key error of the chosen model (the launcher uses it to name every
    /// API key variable when none is set).
    pub key_hint: Option<String>,
    /// `--theme`: a built-in theme name. Beats `MCPE_THEME` and config files.
    pub theme: Option<String>,
    /// `--config`: the only config file read. A missing or invalid file is an error.
    pub config: Option<PathBuf>,
    /// Discovered config files, lowest precedence first. Unreadable ones become warnings.
    pub config_sources: Vec<PathBuf>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            workspace: None,
            model: DEFAULT_TUI_MODEL.to_owned(),
            effort: None,
            models: None,
            mcp_config: None,
            kb: None,
            sessions_dir: None,
            budget_usd: DEFAULT_TUI_BUDGET_USD,
            mock_script: None,
            key_hint: None,
            theme: None,
            config: None,
            config_sources: Vec::new(),
        }
    }
}

/// Resolve the colour theme and config overrides before anything else is opened. Returns the
/// theme selection and the warnings for bad config content. Errors (an explicit `--config` that
/// is missing or invalid, an unknown `--theme` or `MCPE_THEME`) are returned as text.
fn load_theme(
    options: &TuiOptions,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(ThemeSpec, Vec<String>), String> {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    if let Some(path) = &options.config {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read --config {}: {e}", path.display()))?;
        check_theme_json(&text)
            .map_err(|e| format!("--config {} is not valid JSON: {e}", path.display()))?;
        sources.push(ConfigSource {
            path: path.clone(),
            text,
        });
    } else {
        for path in &options.config_sources {
            match std::fs::read_to_string(path) {
                Ok(text) => sources.push(ConfigSource {
                    path: path.clone(),
                    text,
                }),
                Err(e) => warnings.push(format!("{}: cannot read config: {e}", path.display())),
            }
        }
    }
    let choice = ThemeChoice {
        flag: options.theme.clone(),
        env: env(THEME_ENV),
    };
    let home = env("HOME").map(PathBuf::from);
    let read = |path: &Path| std::fs::read_to_string(path).map_err(|e| e.to_string());
    let (spec, more) = load_theme_config(&sources, &choice, &read, home.as_deref())?;
    warnings.extend(more);
    Ok((spec, warnings))
}

/// Everything the TUI needs, built before the terminal is opened.
pub struct TuiSession {
    /// Initial app state.
    pub app: App,
    /// The running agent runtime.
    pub agent: AgentHandle,
    /// Directory holding this session's `transcript.jsonl`.
    pub session_dir: PathBuf,
}

fn unique_session_dir(root: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let first = root.join(format!("session-{stamp}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| root.join(format!("session-{stamp}-{n}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

fn key_missing(name: &str, env: &dyn Fn(&str) -> Option<String>) -> bool {
    env(name).is_none_or(|v| v.trim().is_empty())
}

/// Build the agent, its transcript, the MCP launcher and the initial app state. Every failure is
/// returned as the text after `error: `; nothing here opens the terminal.
///
/// `env` looks up environment variables (the real environment in production). Only the presence
/// of the API key variable of the chosen model is checked here; the key itself is read by the
/// provider client.
pub fn build_tui_session(
    options: &TuiOptions,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<TuiSession, String> {
    let workspace = match &options.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?,
    };
    if !workspace.is_dir() {
        return Err(format!(
            "workspace {} is not a directory",
            workspace.display()
        ));
    }
    let registry = load_registry(options.models.as_deref())?;
    let spec = registry.get(&options.model).map_err(|_| {
        format!(
            "unknown model alias {:?}; known: {}",
            options.model,
            registry.aliases().join(", ")
        )
    })?;
    let effort = match &options.effort {
        Some(text) => Effort::parse(&text.to_ascii_lowercase())
            .filter(|e| spec.effort_supported(*e))
            .ok_or_else(|| {
                let allowed: Vec<&str> = spec.allowed_efforts.iter().map(|e| e.as_str()).collect();
                format!(
                    "model {} does not accept effort {text:?}; allowed: {}",
                    spec.alias,
                    allowed.join(", ")
                )
            })?,
        None => spec.default_effort,
    };
    if !options.budget_usd.is_finite() || options.budget_usd <= 0.0 {
        return Err("--budget-usd must be a positive number".to_owned());
    }
    let (theme_spec, config_warnings) = load_theme(options, env)?;

    // Model clients.
    let mut clients: HashMap<ProviderId, Arc<dyn ModelClient>> = HashMap::new();
    if let Some(dir) = &options.mock_script {
        let mock: Arc<dyn ModelClient> = Arc::new(
            MockModelClient::from_fixture_dir(spec.provider, dir).map_err(|e| e.to_string())?,
        );
        clients.insert(ProviderId::OpenAi, Arc::clone(&mock));
        clients.insert(ProviderId::Gemini, mock);
    } else {
        for model in registry.models() {
            if clients.contains_key(&model.provider) {
                continue;
            }
            let chosen = model.provider == spec.provider;
            if key_missing(&model.api_key_env, env) {
                if chosen {
                    return Err(options.key_hint.clone().unwrap_or_else(|| {
                        format!("environment variable {} is not set", model.api_key_env)
                    }));
                }
                continue;
            }
            let client: Result<Arc<dyn ModelClient>, _> = match model.provider {
                ProviderId::OpenAi => OpenAiClient::from_env(&model.api_key_env, None)
                    .map(|c| Arc::new(c) as Arc<dyn ModelClient>),
                ProviderId::Gemini => GeminiClient::from_env(&model.api_key_env, None)
                    .map(|c| Arc::new(c) as Arc<dyn ModelClient>),
            };
            match client {
                Ok(client) => {
                    clients.insert(model.provider, client);
                }
                Err(e) if chosen => return Err(e.to_string()),
                Err(_) => {}
            }
        }
    }

    // Tools, KB, MCP.
    let builtins = BuiltinTools::new(&workspace, registry.api_key_envs())
        .map_err(|e| format!("cannot open workspace {}: {e}", workspace.display()))?;
    let session_kb = match &options.kb {
        Some(path) => SessionKb::new(ToolKb::read(path).map_err(|e| e.to_string())?),
        None => SessionKb::new(ToolKb {
            schema_version: TOOL_KB_SCHEMA_VERSION.to_owned(),
            kb_id: "session".to_owned(),
            release_id: "session".to_owned(),
            server_version: "live".to_owned(),
            cards: Vec::new(),
        }),
    };
    let sessions_root = options
        .sessions_dir
        .clone()
        .unwrap_or_else(|| workspace.join(".harness").join("sessions"));
    std::fs::create_dir_all(&sessions_root)
        .map_err(|e| format!("cannot create {}: {e}", sessions_root.display()))?;
    let session_dir = unique_session_dir(&sessions_root);
    let transcript = TranscriptWriter::create(
        &session_dir,
        Redactor::from_env_names(registry.api_key_envs()),
    )
    .map_err(|e| e.to_string())?;
    let (launcher, mcp_servers): (Arc<dyn McpLauncher>, Vec<String>) = match &options.mcp_config {
        Some(path) => {
            let config = McpConfig::load(path).map_err(|e| e.to_string())?;
            let launcher = ConfigLauncher::new(config, &session_dir);
            let names = launcher.server_names();
            (Arc::new(launcher), names)
        }
        None => (Arc::new(NoMcp), Vec::new()),
    };

    let ledger = Arc::new(Mutex::new(CostLedger::new(
        options.budget_usd,
        rate_card(&registry),
    )));
    let models_info: Vec<ModelInfo> = registry.models().iter().map(ModelInfo::from_spec).collect();
    let alias = spec.alias.clone();
    let agent = Agent::new(
        AgentConfig::new(&alias, effort, AGENT_INSTRUCTIONS),
        AgentParts {
            models: registry.clone(),
            clients: clients.clone(),
            tools: ToolRegistry::new(NamingMode::Namespaced),
            builtins: Some(builtins),
            ledger,
            policy: ApprovalPolicy::default(),
            transcript: Some(transcript),
        },
    )
    .map_err(|e| e.to_string())?;
    let handle = AgentHandle::spawn(AgentSetup {
        agent,
        models: registry,
        clients,
        session_kb,
        mcp_servers: mcp_servers.clone(),
        launcher,
    })
    .map_err(|e| e.to_string())?;
    let mut app = App::new(AppInit {
        models: models_info,
        model_alias: alias,
        effort,
        budget_usd: options.budget_usd,
        workspace: workspace.display().to_string(),
        mcp_servers,
    });
    app.set_theme_spec(theme_spec);
    app.push_notices(config_warnings);
    Ok(TuiSession {
        app,
        agent: handle,
        session_dir,
    })
}

/// `harness tui [...]`: open the interactive coding agent on the real terminal.
pub fn tui(options: &TuiOptions, err: &mut dyn Write) -> i32 {
    let session = match build_tui_session(options, &real_env) {
        Ok(session) => session,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_ERROR;
        }
    };
    match harness_tui::run(session.app, Box::new(session.agent)) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            EXIT_ERROR
        }
    }
}

/// `mcpe [...]`: resolve the launcher defaults (model by API key, discovered MCP config and KB,
/// workspace and sessions under the current directory) and open the interactive coding agent.
pub fn mcpe(args: crate::launcher::McpeArgs, err: &mut dyn Write) -> i32 {
    let registry = match load_registry(args.models.as_deref()) {
        Ok(registry) => registry,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_ERROR;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            let _ = writeln!(err, "error: cannot read the current directory: {e}");
            return EXIT_ERROR;
        }
    };
    let home = real_env("HOME").map(PathBuf::from);
    let is_file = |path: &Path| path.is_file();
    let options = crate::launcher::resolve_mcpe(
        args,
        &registry,
        &crate::launcher::Lookup {
            var: &real_env,
            is_file: &is_file,
            cwd: &cwd,
            home: home.as_deref(),
        },
    );
    tui(&options, err)
}
