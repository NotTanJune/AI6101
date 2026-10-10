//! Plumbing shared by the HTTP model clients: API key handling, URL checks, retries, error
//! mapping and secret scrubbing.

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use harness_core::redact::Redactor;
use reqwest::{StatusCode, header::RETRY_AFTER};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::ModelError;

/// Longest server error text kept in a [`ModelError`].
pub const MAX_ERROR_CHARS: usize = 500;

/// Request id used when the provider returned none.
pub const UNAVAILABLE_ID: &str = "unavailable";

/// Longest `Retry-After` the clients will honour.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// API key. `Debug` never shows the value; there is no `Display`.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key. Empty keys are rejected.
    pub fn new(key: String) -> Result<Self, ModelError> {
        if key.trim().is_empty() {
            return Err(ModelError::Config("API key is empty".to_owned()));
        }
        Ok(Self(key))
    }

    /// Read a key from an environment variable.
    pub fn from_env(provider: &str, env_name: &str) -> Result<Self, ModelError> {
        match std::env::var(env_name) {
            Ok(value) if !value.trim().is_empty() => Ok(Self(value)),
            _ => Err(ModelError::MissingApiKey {
                provider: provider.to_owned(),
                env_name: env_name.to_owned(),
            }),
        }
    }

    /// The key value. Only for building request headers.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// A redactor that removes this key from text.
    pub(crate) fn redactor(&self) -> Redactor {
        Redactor::new(vec![self.0.clone()])
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

/// Retry behaviour for 429 and 503 answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u32,
    /// Waits used when the server sends no `Retry-After`, indexed by retry number.
    pub backoff: Vec<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            backoff: vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
        }
    }
}

/// Validate and normalise a base URL: `https`, or `http` only for a loopback host (local test
/// servers). An API key is sent to this URL, so anything else is refused.
pub fn check_base_url(what: &str, url: &str) -> Result<String, ModelError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| ModelError::Config(format!("{what} base URL is not a valid URL")))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        bare.eq_ignore_ascii_case("localhost")
            || bare
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => {
            return Err(ModelError::Config(format!(
                "{what} base URL must use https (http is allowed only for loopback hosts) because an API key is sent to it"
            )));
        }
    }
    Ok(url.trim_end_matches('/').to_owned())
}

/// Build the shared HTTP client. `overall_timeout` limits a whole request (use `None` for
/// streaming calls and rely on `read_timeout`).
pub fn build_http(
    overall_timeout: Option<Duration>,
    read_timeout: Duration,
) -> Result<reqwest::Client, ModelError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(read_timeout);
    if let Some(timeout) = overall_timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .map_err(|e| ModelError::Config(format!("cannot build HTTP client: {e}")))
}

/// Redact secrets and truncate to [`MAX_ERROR_CHARS`].
pub fn scrub(redactor: &Redactor, text: &str) -> String {
    redactor
        .redact_str(text)
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

/// Map a `reqwest` failure to a [`ModelError`] without leaking secrets.
pub fn map_reqwest(error: &reqwest::Error, redactor: &Redactor) -> ModelError {
    if error.is_timeout() {
        ModelError::Timeout
    } else {
        // No client puts the key in the URL (it travels in a header); scrub anyway.
        ModelError::Transport(scrub(redactor, &error.to_string()))
    }
}

/// Milliseconds since `started`, saturating.
pub fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Await `future` unless `cancel` fires first.
pub async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ModelError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ModelError::Cancelled),
        value = future => Ok(value),
    }
}

fn error_text(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned())
}

/// POST `body` as JSON, retrying 429 and 503 answers per `policy`. Returns the successful
/// response; any other failure is mapped to a [`ModelError`].
pub async fn post_json_with_retries(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&'static str, String)],
    body: &Value,
    cancel: &CancellationToken,
    redactor: &Redactor,
    policy: &RetryPolicy,
) -> Result<reqwest::Response, ModelError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let mut request = http.post(url).json(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = cancellable(cancel, request.send())
            .await?
            .map_err(|e| map_reqwest(&e, redactor))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retryable =
            status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE;
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs).min(MAX_RETRY_AFTER));
        let text = cancellable(cancel, response.text())
            .await?
            .map_err(|e| map_reqwest(&e, redactor))?;
        if retryable && attempt < policy.max_attempts {
            let index = usize::try_from(attempt - 1).unwrap_or(usize::MAX);
            let wait = retry_after
                .or_else(|| policy.backoff.get(index).or(policy.backoff.last()).copied())
                .unwrap_or(Duration::from_secs(1));
            cancellable(cancel, tokio::time::sleep(wait)).await?;
            continue;
        }
        return Err(ModelError::Http {
            status: status.as_u16(),
            message: scrub(redactor, &error_text(&text)),
        });
    }
}
