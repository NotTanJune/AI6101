//! TOML experiment configuration.
//!
//! The config never holds secrets: credentials are referenced by environment variable NAME
//! (`*_env` fields). `strategies` lives under `[run]`.
//!
//! ```toml
//! [run]
//! name = "smoke"
//! seed = 42
//! repetitions = 1
//! strategies = ["no_update", "full_rewrite", "selective_patch"]
//!
//! [inputs]
//! adapter = "fixture"             # or "mcpevol" (MCPEvol-Bench; reads the two env vars below)
//! root = "smoke"                  # fixture only, relative to this file
//! catalog_env = "MCP_DATA_DIR"    # mcpevol: data dir with test_cases/status_dict.json, mutation/HYBRID/...
//! eval_env = "EVAL_DATA_DIR"      # mcpevol: data dir with test_cases/{test_cases,chosen_idx}.json
//!
//! [versions]
//! source = "early"
//! target = "late"
//! stages = { early = "v1", middle = "v2", late = "v3" }   # optional
//!
//! [tasks]
//! ids = ["task-001"]
//!
//! [provider]
//! kind = "mock"                   # mock | openai | gemini | local
//! model = "mock-model"
//! # base_url, api_key_env, temperature, top_p, max_output_tokens, seed are optional
//! # mock_responses_dir = "canned"   # mock only: directory of <purpose>.json responses
//!
//! [judge]
//! version = "judge-v1"
//!
//! [budget]
//! cap_usd = 100.0
//! # [budget.rate_card] id/date + [[budget.rate_card.entries]] (required for paid providers)
//!
//! [runner]
//! kind = "fixture"                # fixture | process
//! fixture_dir = "smoke/runner"    # or: executable = "python", fixed_args = ["-m", "runner"]
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cost::{ProviderKind as CostProviderKind, RateCard};
use crate::redact::Redactor;

/// Adaptation strategy compared by the experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Keep the original Tool KB.
    NoUpdate,
    /// Regenerate the whole KB for the target version.
    FullRewrite,
    /// Update only cards and fields tied to detected changes.
    SelectivePatch,
}

impl Strategy {
    /// All strategies in canonical order.
    pub const ALL: [Strategy; 3] = [Self::NoUpdate, Self::FullRewrite, Self::SelectivePatch];

    /// Wire name (`no_update`, `full_rewrite`, `selective_patch`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoUpdate => "no_update",
            Self::FullRewrite => "full_rewrite",
            Self::SelectivePatch => "selective_patch",
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Model provider kind as written in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Deterministic in-process mock.
    Mock,
    /// OpenAI API (paid).
    OpenAi,
    /// Gemini API (paid).
    Gemini,
    /// Local OpenAI-compatible endpoint.
    Local,
}

impl ProviderKind {
    /// Provider name used in rate cards, events and reservations.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Local => "local",
        }
    }

    /// How this provider is accounted for by the cost ledger.
    pub fn cost_kind(self) -> CostProviderKind {
        match self {
            Self::Mock => CostProviderKind::Mock,
            Self::Local => CostProviderKind::Local,
            Self::OpenAi | Self::Gemini => CostProviderKind::Paid,
        }
    }
}

/// `[run]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSection {
    /// Run name.
    pub name: String,
    /// Experiment seed.
    pub seed: u64,
    /// Repetitions per strategy.
    pub repetitions: u32,
    /// Strategies to run.
    pub strategies: Vec<Strategy>,
}

/// Input adapter kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterKind {
    /// Checked-in fixtures.
    Fixture,
    /// MCPEvol-Bench data directories named by `catalog_env` and `eval_env`.
    Mcpevol,
}

/// `[inputs]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputsSection {
    /// Adapter kind.
    pub adapter: AdapterKind,
    /// Fixture root (resolved against the config file's directory on load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// Env var NAME holding the catalog directory.
    #[serde(default = "default_catalog_env")]
    pub catalog_env: String,
    /// Env var NAME holding the evaluation data directory.
    #[serde(default = "default_eval_env")]
    pub eval_env: String,
}

fn default_catalog_env() -> String {
    "MCP_DATA_DIR".to_owned()
}

fn default_eval_env() -> String {
    "EVAL_DATA_DIR".to_owned()
}

/// Optional mapping of version stages to version labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stages {
    /// Label of the early version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early: Option<String>,
    /// Label of the middle version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middle: Option<String>,
    /// Label of the late version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late: Option<String>,
}

/// `[versions]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionsSection {
    /// Source version label.
    pub source: String,
    /// Target version label.
    pub target: String,
    /// Optional stage-to-label mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages: Option<Stages>,
}

/// `[tasks]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TasksSection {
    /// Fixed task ids.
    pub ids: Vec<String>,
}

/// `[provider]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSection {
    /// Provider kind.
    pub kind: ProviderKind,
    /// Model id.
    pub model: String,
    /// Base URL (required for `local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Env var NAME holding the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Output token limit (required for paid providers so calls can be reserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Provider-side sampling seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Directory of canned mock responses (`<purpose>.json`), only used by the `mock` kind.
    /// Resolved against the config file's directory on load; must exist for `mock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock_responses_dir: Option<PathBuf>,
}

/// `[judge]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeSection {
    /// Judge version.
    pub version: String,
}

/// `[budget]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSection {
    /// Local dispatch cap in USD.
    pub cap_usd: f64,
    /// Rate card (required for paid providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_card: Option<RateCard>,
}

/// Runner kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerKind {
    /// Replay checked-in fixture logs.
    Fixture,
    /// Spawn an external process.
    Process,
}

/// `[runner]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerSection {
    /// Runner kind.
    pub kind: RunnerKind,
    /// Directory of fixture logs (resolved against the config file's directory on load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture_dir: Option<PathBuf>,
    /// Executable to spawn (relative paths with a separator are resolved on load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Fixed arguments, passed as an argv array (never a shell string).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed_args: Vec<String>,
}

/// Complete experiment configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[run]`
    pub run: RunSection,
    /// `[inputs]`
    pub inputs: InputsSection,
    /// `[versions]`
    pub versions: VersionsSection,
    /// `[tasks]`
    pub tasks: TasksSection,
    /// `[provider]`
    pub provider: ProviderSection,
    /// `[judge]`
    pub judge: JudgeSection,
    /// `[budget]`
    pub budget: BudgetSection,
    /// `[runner]`
    pub runner: RunnerSection,
    /// Directory relative paths were resolved against.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

/// One configuration problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Dotted path of the offending field, for example `provider.api_key_env`.
    pub field: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Failure to load or accept a configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config {}: {source}", path.display())]
    Io {
        /// Config path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid for the config schema (syntax, unknown key/strategy, wrong type).
    #[error("invalid config {}: {message}", path.display())]
    Parse {
        /// Config path.
        path: PathBuf,
        /// Parser message.
        message: String,
    },
    /// The config parsed but has problems.
    #[error("config has {} problem(s): {}", problems.len(), join_problems(problems))]
    Invalid {
        /// All problems found.
        problems: Vec<Problem>,
    },
}

fn join_problems(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(Problem::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl Config {
    /// Read and parse a config file, resolving relative paths against its directory.
    /// Does not run [`Config::validate`].
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let base = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Self::from_toml_str(&text, base).map_err(|message| ConfigError::Parse {
            path: path.to_owned(),
            message,
        })
    }

    /// Parse TOML text, resolving relative paths against `base_dir`.
    pub fn from_toml_str(text: &str, base_dir: impl AsRef<Path>) -> Result<Config, String> {
        let mut config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.base_dir = base_dir.as_ref().to_owned();
        config.resolve_paths();
        Ok(config)
    }

    /// Load and validate; fails with every problem found.
    pub fn load_validated(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let config = Self::load(path)?;
        let problems = config.validate();
        if problems.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid { problems })
        }
    }

    fn resolve_paths(&mut self) {
        let base = self.base_dir.clone();
        let join = |p: &Path| {
            if p.is_absolute() {
                p.to_owned()
            } else {
                base.join(p)
            }
        };
        if let Some(root) = &self.inputs.root {
            self.inputs.root = Some(join(root));
        }
        if let Some(dir) = &self.runner.fixture_dir {
            self.runner.fixture_dir = Some(join(dir));
        }
        if let Some(dir) = &self.provider.mock_responses_dir {
            self.provider.mock_responses_dir = Some(join(dir));
        }
        if let Some(exe) = &self.runner.executable {
            let has_separator = exe.contains('/') || exe.contains(std::path::MAIN_SEPARATOR);
            let path = Path::new(exe);
            if has_separator && !path.is_absolute() {
                self.runner.executable = Some(base.join(path).to_string_lossy().into_owned());
            }
        }
    }

    /// Check the config and return every problem (empty means valid). No network or provider
    /// access; only fixture paths are checked on disk.
    pub fn validate(&self) -> Vec<Problem> {
        let mut v = Validation::default();
        self.validate_run(&mut v);
        self.validate_inputs(&mut v);
        self.validate_versions(&mut v);
        self.validate_tasks(&mut v);
        self.validate_provider(&mut v);
        self.validate_budget(&mut v);
        self.validate_runner(&mut v);
        if self.judge.version.trim().is_empty() {
            v.add("judge.version", "must not be empty");
        }
        v.problems
    }

    fn validate_run(&self, v: &mut Validation) {
        if self.run.name.trim().is_empty() {
            v.add("run.name", "must not be empty");
        }
        if self.run.repetitions == 0 {
            v.add("run.repetitions", "must be at least 1");
        }
        if self.run.strategies.is_empty() {
            v.add("run.strategies", "must list at least one strategy");
        }
        let unique: BTreeSet<_> = self.run.strategies.iter().collect();
        if unique.len() != self.run.strategies.len() {
            v.add("run.strategies", "must not contain duplicates");
        }
    }

    fn validate_inputs(&self, v: &mut Validation) {
        match self.inputs.adapter {
            AdapterKind::Fixture => match &self.inputs.root {
                None => v.add("inputs.root", "required for the fixture adapter"),
                Some(root) if !root.is_dir() => v.add(
                    "inputs.root",
                    format!("directory does not exist: {}", root.display()),
                ),
                Some(_) => {}
            },
            // The environment variables and directories are checked where the adapter is
            // built (`pipeline::build_adapter`), because validation here never reads the env.
            AdapterKind::Mcpevol => {}
        }
        v.check_env_name("inputs.catalog_env", &self.inputs.catalog_env);
        v.check_env_name("inputs.eval_env", &self.inputs.eval_env);
    }

    fn validate_versions(&self, v: &mut Validation) {
        if self.versions.source.trim().is_empty() {
            v.add("versions.source", "must not be empty");
        }
        if self.versions.target.trim().is_empty() {
            v.add("versions.target", "must not be empty");
        }
        if self.versions.source == self.versions.target {
            v.add("versions.target", "must differ from versions.source");
        }
    }

    fn validate_tasks(&self, v: &mut Validation) {
        if self.tasks.ids.is_empty() {
            v.add("tasks.ids", "must list at least one task id");
        }
        if self.tasks.ids.iter().any(|id| id.trim().is_empty()) {
            v.add("tasks.ids", "task ids must not be empty");
        }
        let unique: BTreeSet<_> = self.tasks.ids.iter().collect();
        if unique.len() != self.tasks.ids.len() {
            v.add("tasks.ids", "must not contain duplicates");
        }
    }

    fn validate_provider(&self, v: &mut Validation) {
        let p = &self.provider;
        if p.model.trim().is_empty() {
            v.add("provider.model", "must not be empty");
        }
        if let Some(env) = &p.api_key_env {
            v.check_env_name("provider.api_key_env", env);
        }
        if let Some(t) = p.temperature
            && !(t.is_finite() && (0.0..=2.0).contains(&t))
        {
            v.add("provider.temperature", "must be between 0 and 2");
        }
        if let Some(t) = p.top_p
            && !(t.is_finite() && t > 0.0 && t <= 1.0)
        {
            v.add("provider.top_p", "must be in (0, 1]");
        }
        if p.max_output_tokens == Some(0) {
            v.add("provider.max_output_tokens", "must be greater than 0");
        }
        if let Some(url) = &p.base_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                v.add("provider.base_url", "must start with http:// or https://");
            } else if url_has_credentials(url) {
                v.add(
                    "provider.base_url",
                    "must not embed credentials; use api_key_env",
                );
            }
        }
        match p.kind {
            ProviderKind::Mock => match &p.mock_responses_dir {
                Some(dir) if !dir.is_dir() => v.add(
                    "provider.mock_responses_dir",
                    format!("directory does not exist: {}", dir.display()),
                ),
                _ => {}
            },
            ProviderKind::Local => {
                if p.base_url.is_none() {
                    v.add("provider.base_url", "required for a local provider");
                }
            }
            ProviderKind::OpenAi | ProviderKind::Gemini => {
                if p.api_key_env.is_none() {
                    v.add(
                        "provider.api_key_env",
                        "required for a paid provider (name of the env var holding the key)",
                    );
                }
                if p.max_output_tokens.is_none() {
                    v.add(
                        "provider.max_output_tokens",
                        "required for a paid provider so calls can be reserved",
                    );
                }
                let priced = self
                    .budget
                    .rate_card
                    .as_ref()
                    .and_then(|rc| rc.find(p.kind.name(), &p.model))
                    .is_some();
                if !priced {
                    v.add(
                        "budget.rate_card",
                        format!(
                            "no rate entry for paid provider {:?} model {:?}; paid calls are blocked without a rate",
                            p.kind.name(),
                            p.model
                        ),
                    );
                }
            }
        }
    }

    fn validate_budget(&self, v: &mut Validation) {
        let cap = self.budget.cap_usd;
        if !(cap.is_finite() && cap > 0.0) {
            v.add("budget.cap_usd", "must be a positive number");
        }
        let Some(card) = &self.budget.rate_card else {
            return;
        };
        if card.id.trim().is_empty() {
            v.add("budget.rate_card.id", "must not be empty");
        }
        if !is_iso_date(&card.date) {
            v.add("budget.rate_card.date", "must be a date like 2026-09-01");
        }
        let mut seen = BTreeSet::new();
        for (i, e) in card.entries.iter().enumerate() {
            let field = format!("budget.rate_card.entries[{i}]");
            if e.provider.trim().is_empty() || e.model.trim().is_empty() {
                v.add(&field, "provider and model must not be empty");
            }
            for (name, rate) in [
                ("input_usd_per_mtok", e.input_usd_per_mtok),
                ("output_usd_per_mtok", e.output_usd_per_mtok),
            ] {
                if !(rate.is_finite() && rate >= 0.0) {
                    v.add(&format!("{field}.{name}"), "must be a non-negative number");
                }
            }
            if !seen.insert((&e.provider, &e.model)) {
                v.add(&field, "duplicate provider/model entry");
            }
        }
    }

    fn validate_runner(&self, v: &mut Validation) {
        let r = &self.runner;
        match r.kind {
            RunnerKind::Fixture => match &r.fixture_dir {
                None => v.add("runner.fixture_dir", "required for the fixture runner"),
                Some(dir) if !dir.is_dir() => v.add(
                    "runner.fixture_dir",
                    format!("directory does not exist: {}", dir.display()),
                ),
                Some(_) => {}
            },
            RunnerKind::Process => {
                if r.executable.as_deref().is_none_or(|e| e.trim().is_empty()) {
                    v.add("runner.executable", "required for the process runner");
                }
            }
        }
        let redactor = Redactor::default();
        for (i, arg) in r.fixed_args.iter().enumerate() {
            if redactor.looks_like_secret(arg) {
                v.add(
                    &format!("runner.fixed_args[{i}]"),
                    "looks like a secret; secrets must never be passed in argv",
                );
            }
        }
    }
}

#[derive(Default)]
struct Validation {
    problems: Vec<Problem>,
}

impl Validation {
    fn add(&mut self, field: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            field: field.to_owned(),
            message: message.into(),
        });
    }

    /// `*_env` fields hold the NAME of an environment variable, never a value.
    fn check_env_name(&mut self, field: &str, value: &str) {
        if Redactor::default().looks_like_secret(value) {
            self.add(
                field,
                "looks like a secret value; config must hold an env var NAME, not a secret",
            );
        } else if !is_env_name(value) {
            self.add(
                field,
                "must be an environment variable name (letters, digits, underscore; not starting with a digit)",
            );
        }
    }
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

fn url_has_credentials(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}
