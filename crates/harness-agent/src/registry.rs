//! Model registry: which models the harness can talk to, their effort settings and prices.
//!
//! The registry is loaded from `configs/models.toml`. Every entry carries the full price set,
//! which [`crate::pricing`] converts into the core cost ledger's rate card.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Reasoning effort requested from a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// No reasoning.
    None,
    /// Minimal reasoning.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort.
    Xhigh,
    /// Maximum effort.
    Max,
}

impl Effort {
    /// The lowercase wire name, as accepted by the provider APIs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parse a lowercase effort name.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProviderId {
    /// OpenAI (Responses API).
    #[serde(rename = "openai")]
    OpenAi,
    /// Google Gemini (Interactions API).
    #[serde(rename = "gemini")]
    Gemini,
}

impl ProviderId {
    /// Provider name used in rate cards and cost records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }
}

/// Price set for one model, in USD per million tokens (mirrors the core `RateEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// USD per million input tokens.
    pub input_usd_per_mtok: f64,
    /// USD per million output tokens (including reasoning).
    pub output_usd_per_mtok: f64,
    /// USD per million cached input tokens.
    #[serde(default)]
    pub cached_input_usd_per_mtok: Option<f64>,
    /// USD per million cache-write input tokens.
    #[serde(default)]
    pub cache_write_usd_per_mtok: Option<f64>,
    /// Input token count above which the long-context rates apply.
    #[serde(default)]
    pub long_context_threshold_tokens: Option<u64>,
    /// Long-context USD per million input tokens.
    #[serde(default)]
    pub long_input_usd_per_mtok: Option<f64>,
    /// Long-context USD per million cached input tokens.
    #[serde(default)]
    pub long_cached_input_usd_per_mtok: Option<f64>,
    /// Long-context USD per million cache-write input tokens.
    #[serde(default)]
    pub long_cache_write_usd_per_mtok: Option<f64>,
    /// Long-context USD per million output tokens.
    #[serde(default)]
    pub long_output_usd_per_mtok: Option<f64>,
    /// ISO date the prices were taken.
    #[serde(default)]
    pub price_date: Option<String>,
}

/// One model the harness can use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// Short name used in the UI and on the command line (`luna`, `sol`, ...).
    pub alias: String,
    /// Provider.
    pub provider: ProviderId,
    /// Model ID sent to the provider API.
    pub model_id: String,
    /// Environment variable holding the API key.
    pub api_key_env: String,
    /// Efforts the model accepts.
    pub allowed_efforts: Vec<Effort>,
    /// Effort used when the user gives none.
    pub default_effort: Effort,
    /// Whether the model accepts temperature, top_p and seed.
    pub accepts_sampling: bool,
    /// Largest output token budget the model allows.
    pub max_output_tokens: u32,
    /// Prices.
    pub price: Price,
}

impl ModelSpec {
    /// Whether the model accepts the given effort.
    pub fn effort_supported(&self, effort: Effort) -> bool {
        self.allowed_efforts.contains(&effort)
    }
}

/// Registry loading or validation failure.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The file could not be read.
    #[error("cannot read model registry {path}: {message}")]
    Io {
        /// File path.
        path: PathBuf,
        /// OS error text.
        message: String,
    },
    /// The file is not valid TOML for the registry.
    #[error("invalid model registry: {0}")]
    Parse(String),
    /// The registry parsed but is inconsistent.
    #[error("invalid model registry: {0}")]
    Invalid(String),
    /// No model has this alias.
    #[error("unknown model alias {0:?}")]
    UnknownAlias(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    models: Vec<ModelSpec>,
}

/// Validated set of models.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRegistry {
    models: Vec<ModelSpec>,
}

impl ModelRegistry {
    /// Load and validate a registry file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| RegistryError::Io {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        Self::from_toml_str(&text)
    }

    /// Parse and validate registry TOML.
    pub fn from_toml_str(text: &str) -> Result<Self, RegistryError> {
        let file: RegistryFile =
            toml::from_str(text).map_err(|e| RegistryError::Parse(e.to_string()))?;
        let registry = Self {
            models: file.models,
        };
        registry.validate()?;
        Ok(registry)
    }

    /// Build a registry from specs, validating it.
    pub fn from_specs(models: Vec<ModelSpec>) -> Result<Self, RegistryError> {
        let registry = Self { models };
        registry.validate()?;
        Ok(registry)
    }

    /// Check the registry: aliases unique, default effort allowed, prices usable.
    pub fn validate(&self) -> Result<(), RegistryError> {
        if self.models.is_empty() {
            return Err(RegistryError::Invalid("no models defined".to_owned()));
        }
        let mut seen = BTreeSet::new();
        for spec in &self.models {
            let alias = &spec.alias;
            if alias.is_empty() || spec.model_id.is_empty() || spec.api_key_env.is_empty() {
                return Err(RegistryError::Invalid(format!(
                    "model {alias:?}: alias, model_id and api_key_env must be non-empty"
                )));
            }
            if !seen.insert(alias.clone()) {
                return Err(RegistryError::Invalid(format!("duplicate alias {alias:?}")));
            }
            if spec.allowed_efforts.is_empty() {
                return Err(RegistryError::Invalid(format!(
                    "model {alias:?}: allowed_efforts is empty"
                )));
            }
            if !spec.effort_supported(spec.default_effort) {
                return Err(RegistryError::Invalid(format!(
                    "model {alias:?}: default effort {} is not in allowed_efforts",
                    spec.default_effort
                )));
            }
            if spec.max_output_tokens == 0 {
                return Err(RegistryError::Invalid(format!(
                    "model {alias:?}: max_output_tokens must be positive"
                )));
            }
            validate_price(alias, &spec.price)?;
        }
        Ok(())
    }

    /// Model with this alias.
    pub fn get(&self, alias: &str) -> Result<&ModelSpec, RegistryError> {
        self.models
            .iter()
            .find(|m| m.alias == alias)
            .ok_or_else(|| RegistryError::UnknownAlias(alias.to_owned()))
    }

    /// All models in file order.
    pub fn models(&self) -> &[ModelSpec] {
        &self.models
    }

    /// Every alias in file order.
    pub fn aliases(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.alias.as_str()).collect()
    }

    /// Names of every API key environment variable named by a model.
    pub fn api_key_envs(&self) -> BTreeSet<String> {
        self.models.iter().map(|m| m.api_key_env.clone()).collect()
    }
}

fn validate_price(alias: &str, price: &Price) -> Result<(), RegistryError> {
    let rates = [
        ("input_usd_per_mtok", Some(price.input_usd_per_mtok)),
        ("output_usd_per_mtok", Some(price.output_usd_per_mtok)),
        ("cached_input_usd_per_mtok", price.cached_input_usd_per_mtok),
        ("cache_write_usd_per_mtok", price.cache_write_usd_per_mtok),
        ("long_input_usd_per_mtok", price.long_input_usd_per_mtok),
        (
            "long_cached_input_usd_per_mtok",
            price.long_cached_input_usd_per_mtok,
        ),
        (
            "long_cache_write_usd_per_mtok",
            price.long_cache_write_usd_per_mtok,
        ),
        ("long_output_usd_per_mtok", price.long_output_usd_per_mtok),
    ];
    for (name, value) in rates {
        if let Some(v) = value
            && !(v.is_finite() && v >= 0.0)
        {
            return Err(RegistryError::Invalid(format!(
                "model {alias:?}: price {name} must be finite and >= 0"
            )));
        }
    }
    if price.input_usd_per_mtok <= 0.0 || price.output_usd_per_mtok <= 0.0 {
        return Err(RegistryError::Invalid(format!(
            "model {alias:?}: input and output prices must be positive"
        )));
    }
    Ok(())
}
