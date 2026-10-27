//! Adapter from the async [`ModelClient`] to core's blocking [`Provider`] trait.
//!
//! Core's adaptation strategies run synchronously through `MeteredProvider`. [`BlockingProvider`]
//! lets them use the new OpenAI and Gemini clients: each `complete` call builds a one-message
//! [`ModelRequest`] without tools and blocks on the client with a runtime handle.
//!
//! **It must be called from a thread that is not a runtime worker.** Run the adaptation inside
//! `tokio::task::spawn_blocking`, otherwise `Handle::block_on` panics.

use std::sync::Arc;

use harness_core::cost::ProviderKind;
use harness_core::provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, UsageReport,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use crate::model::{Item, ModelClient, ModelError, ModelRequest};
use crate::registry::{Effort, ModelSpec};

/// Blocking [`Provider`] backed by a [`ModelClient`].
///
/// Usage is reported as input tokens and output tokens including reasoning; cached-token
/// detail does not pass through core's `UsageReport`, so cached input is billed as plain input
/// on this path (a slight overestimate).
pub struct BlockingProvider {
    client: Arc<dyn ModelClient>,
    spec: ModelSpec,
    effort: Effort,
    handle: Handle,
    cancellation: CancellationToken,
}

impl std::fmt::Debug for BlockingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingProvider")
            .field("model", &self.spec.alias)
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}

impl BlockingProvider {
    /// Wrap a client. `handle` is the runtime the client's futures run on.
    pub fn new(
        client: Arc<dyn ModelClient>,
        spec: ModelSpec,
        effort: Effort,
        handle: Handle,
    ) -> Self {
        Self {
            client,
            spec,
            effort,
            handle,
            cancellation: CancellationToken::new(),
        }
    }

    /// Cancel pending adaptation calls with the owning session.
    ///
    /// The bridge stops waiting even if the client ignores cancellation. A request
    /// already sent may still be billed, so cancellation preserves its cost reservation.
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = token;
        self
    }
}

fn map_error(error: ModelError) -> ProviderError {
    match error {
        ModelError::MissingApiKey { provider, env_name } => {
            ProviderError::MissingApiKey { provider, env_name }
        }
        ModelError::Config(message) => ProviderError::Config(message),
        ModelError::Http { status, message } => ProviderError::Http { status, message },
        ModelError::Timeout => ProviderError::Timeout,
        ModelError::Transport(message) => ProviderError::Transport(message),
        ModelError::InvalidResponse(message) => ProviderError::InvalidResponse(message),
        ModelError::Mock(message) => ProviderError::Mock(message),
        // A cancelled call may already have been billed, so report it as billing-uncertain
        // (core keeps the reservation against the cap). This covers both external
        // session cancellation and a client that returns `Cancelled` on its own.
        ModelError::Cancelled => ProviderError::InvalidResponse(
            "model call cancelled before a response was received".to_owned(),
        ),
        e @ ModelError::UnsupportedEffort { .. } => ProviderError::Config(e.to_string()),
    }
}

impl Provider for BlockingProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Paid
    }

    fn id(&self) -> &str {
        self.spec.provider.as_str()
    }

    fn model(&self) -> &str {
        &self.spec.model_id
    }

    fn complete(&mut self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.cancellation.is_cancelled() {
            return Err(map_error(ModelError::Cancelled));
        }
        let history = [Item::User {
            text: req.user.clone(),
        }];
        let request = ModelRequest {
            spec: &self.spec,
            effort: self.effort,
            instructions: &req.system,
            history: &history,
            tools: &[],
            max_output_tokens: req.max_output_tokens,
            json_schema: req.json_schema.as_ref(),
        };
        // Stream events are not used on this path; the receiver is dropped.
        let (tx, _rx) = unbounded_channel();
        let response = self
            .handle
            .block_on(async {
                tokio::select! {
                    biased;
                    () = self.cancellation.cancelled() => Err(ModelError::Cancelled),
                    response = self.client.send(request, tx, self.cancellation.clone()) => response,
                }
            })
            .map_err(map_error)?;
        let has_usage =
            response.usage.input.is_some() || response.usage.output_including_reasoning.is_some();
        Ok(CompletionResponse {
            text: response.turn.text,
            usage: has_usage.then_some(UsageReport {
                input_tokens: response.usage.input,
                output_tokens: response.usage.output_including_reasoning,
            }),
            request_id: response.request_id,
            latency_ms: response.latency_ms,
        })
    }
}
