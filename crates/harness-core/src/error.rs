//! Crate-level error type that aggregates the module errors.

use std::path::PathBuf;

use crate::config::ConfigError;
use crate::cost::CostError;
use crate::events::EventError;
use crate::provider::ProviderError;
use crate::runner::RunnerError;

/// Result alias for operations that can fail with any harness error.
pub type Result<T> = std::result::Result<T, HarnessError>;

/// Top-level harness error. Module-specific errors convert into it with `?`.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Filesystem failure.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path involved in the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// JSON (de)serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// A JSON document failed schema validation.
    #[error("{what} failed schema validation: {}", errors.join("; "))]
    Schema {
        /// Which contract was being validated.
        what: &'static str,
        /// Human-readable validation errors.
        errors: Vec<String>,
    },
    /// Run-event failure.
    #[error(transparent)]
    Event(#[from] EventError),
    /// Configuration failure.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Cost accounting failure.
    #[error(transparent)]
    Cost(#[from] CostError),
    /// Provider construction failure.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Benchmark runner construction or execution failure.
    #[error(transparent)]
    Runner(#[from] RunnerError),
    /// Pipeline-level failure that has no more specific type.
    #[error("{0}")]
    Pipeline(String),
}

impl HarnessError {
    /// Wrap an I/O error with the path it concerns.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
