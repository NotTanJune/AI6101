//! MCP client: server configuration, connections and tool calls.
//!
//! [`McpConnection`] wraps an `rmcp` client service. It can talk to a child process over stdio
//! (the standard `mcp.json` shape), to a streamable HTTP endpoint, or to any `(AsyncRead,
//! AsyncWrite)` pair (used by tests with an in-process server). Child stderr goes to a log
//! file, never to the terminal.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// Default per-call timeout.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a graceful shutdown may take before the child is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// MCP failure. Messages never contain environment values from the server config.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The configuration is invalid.
    #[error("invalid MCP configuration: {0}")]
    Config(String),
    /// The server process could not be started.
    #[error("cannot start MCP server {server}: {message}")]
    Spawn {
        /// Server name.
        server: String,
        /// OS error text.
        message: String,
    },
    /// The handshake or transport failed.
    #[error("cannot connect to MCP server {server}: {message}")]
    Connect {
        /// Server name.
        server: String,
        /// Error text.
        message: String,
    },
    /// A request failed after the connection was established.
    #[error("MCP server {server} request failed: {message}")]
    Request {
        /// Server name.
        server: String,
        /// Error text.
        message: String,
    },
    /// A tool call exceeded the timeout.
    #[error("MCP tool {tool} on {server} timed out after {secs}s")]
    Timeout {
        /// Server name.
        server: String,
        /// Tool name.
        tool: String,
        /// Timeout in seconds.
        secs: u64,
    },
    /// The connection was shut down.
    #[error("MCP server {0} is not connected")]
    Closed(String),
    /// Tool arguments are not a JSON object.
    #[error("tool arguments must be a JSON object: {0}")]
    InvalidArguments(String),
}

/// Configuration of one MCP server.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Executable for a stdio server.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments for a stdio server.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for a stdio server.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// URL of a streamable HTTP server.
    #[serde(default)]
    pub url: Option<String>,
    /// Explicit permission and reset contract for disposable live canary calls.
    #[serde(default)]
    pub canary: Option<McpCanaryConfig>,
}

/// Per-server contract required before a live canary can call any MCP tool.
///
/// Set `isolated` only when every state touched by the probes is disposable. The reset
/// tool must restore that state before each probe. These settings are an operator
/// attestation, not an automatic guarantee about an external server's implementation.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCanaryConfig {
    /// Operator attestation that all probe state is disposable and isolated.
    #[serde(default)]
    pub isolated: bool,
    /// Live tool that restores the disposable baseline before every probe.
    #[serde(default)]
    pub reset_tool: String,
    /// JSON object passed to the reset tool. Values may contain secrets.
    #[serde(default = "empty_arguments")]
    pub reset_arguments: Value,
}

impl Default for McpCanaryConfig {
    fn default() -> Self {
        Self {
            isolated: false,
            reset_tool: String::new(),
            reset_arguments: empty_arguments(),
        }
    }
}

fn empty_arguments() -> Value {
    Value::Object(Default::default())
}

impl fmt::Debug for McpCanaryConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpCanaryConfig")
            .field("isolated", &self.isolated)
            .field("reset_tool", &self.reset_tool)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Environment values may be secrets: show names only.
        f.debug_struct("McpServerConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("url", &self.url)
            .field("canary", &self.canary)
            .finish()
    }
}

/// The standard `mcp.json` document: `{"mcpServers": {"<name>": {...}}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct McpConfig {
    /// Servers by name.
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpConfig {
    /// Parse and validate `mcp.json` text.
    pub fn from_json_str(text: &str) -> Result<Self, McpError> {
        let config: Self =
            serde_json::from_str(text).map_err(|e| McpError::Config(e.to_string()))?;
        for (name, server) in &config.servers {
            match (&server.command, &server.url) {
                (Some(_), Some(_)) => {
                    return Err(McpError::Config(format!(
                        "server {name:?} sets both command and url"
                    )));
                }
                (None, None) => {
                    return Err(McpError::Config(format!(
                        "server {name:?} needs a command or a url"
                    )));
                }
                _ => {}
            }
        }
        Ok(config)
    }

    /// Load an `mcp.json` file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, McpError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| McpError::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::from_json_str(&text)
    }
}

/// A tool as the live server lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTool {
    /// Tool name.
    pub name: String,
    /// Description, empty when the server gives none.
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
    /// JSON Schema of the structured result, when the server declares one.
    pub output_schema: Option<Value>,
}

/// Result of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    /// Text content joined with newlines. Non-text parts appear as `[<type> content omitted]`.
    pub text: String,
    /// Whether the server flagged the result as an error.
    pub is_error: bool,
}

/// A connected MCP server.
pub struct McpConnection {
    name: String,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
    call_timeout: Duration,
}

impl fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

impl McpConnection {
    /// Spawn a stdio server and connect to it. The child's stderr is written to `stderr_log`
    /// (the parent directory is created).
    pub async fn connect_stdio(
        name: &str,
        config: &McpServerConfig,
        stderr_log: &Path,
        call_timeout: Duration,
    ) -> Result<Self, McpError> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| McpError::Config(format!("server {name:?} has no command (stdio)")))?;
        if let Some(parent) = stderr_log.parent() {
            std::fs::create_dir_all(parent).map_err(|e| McpError::Spawn {
                server: name.to_owned(),
                message: format!("cannot create log directory: {e}"),
            })?;
        }
        let log = std::fs::File::create(stderr_log).map_err(|e| McpError::Spawn {
            server: name.to_owned(),
            message: format!("cannot create stderr log: {e}"),
        })?;
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args).envs(&config.env);
        let (transport, _stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| McpError::Spawn {
                server: name.to_owned(),
                message: e.to_string(),
            })?;
        Self::connect_transport(name, transport, call_timeout).await
    }

    /// Connect to a streamable HTTP server.
    pub async fn connect_http(
        name: &str,
        url: &str,
        call_timeout: Duration,
    ) -> Result<Self, McpError> {
        let transport = StreamableHttpClientTransport::from_uri(url.to_owned());
        Self::connect_transport(name, transport, call_timeout).await
    }

    /// Connect over any transport `rmcp` accepts, for example a `tokio::io::duplex` half.
    pub async fn connect_transport<T, E, A>(
        name: &str,
        transport: T,
        call_timeout: Duration,
    ) -> Result<Self, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let service = ().serve(transport).await.map_err(|e| McpError::Connect {
            server: name.to_owned(),
            message: e.to_string(),
        })?;
        Ok(Self {
            name: name.to_owned(),
            service: Mutex::new(Some(service)),
            call_timeout,
        })
    }

    /// Server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    async fn peer(&self) -> Result<rmcp::service::Peer<RoleClient>, McpError> {
        let guard = self.service.lock().await;
        guard
            .as_ref()
            .map(|s| s.peer().clone())
            .ok_or_else(|| McpError::Closed(self.name.clone()))
    }

    /// List every tool the server currently exposes.
    pub async fn list_tools(&self) -> Result<Vec<LiveTool>, McpError> {
        let peer = self.peer().await?;
        let tools = tokio::time::timeout(self.call_timeout, peer.list_all_tools())
            .await
            .map_err(|_| McpError::Timeout {
                server: self.name.clone(),
                tool: "tools/list".to_owned(),
                secs: self.call_timeout.as_secs(),
            })?
            .map_err(|e| McpError::Request {
                server: self.name.clone(),
                message: e.to_string(),
            })?;
        Ok(tools
            .into_iter()
            .map(|t| LiveTool {
                name: t.name.into_owned(),
                description: t
                    .description
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
                output_schema: t.output_schema.map(|s| Value::Object((*s).clone())),
            })
            .collect())
    }

    /// Call a tool. `arguments` must be a JSON object (or `null` for none).
    pub async fn call_tool(&self, tool: &str, arguments: &Value) -> Result<CallOutcome, McpError> {
        let mut params = CallToolRequestParams::new(tool.to_owned());
        match arguments {
            Value::Null => {}
            Value::Object(map) => params = params.with_arguments(map.clone()),
            other => return Err(McpError::InvalidArguments(describe_json(other))),
        }
        let peer = self.peer().await?;
        let result = tokio::time::timeout(self.call_timeout, peer.call_tool(params))
            .await
            .map_err(|_| McpError::Timeout {
                server: self.name.clone(),
                tool: tool.to_owned(),
                secs: self.call_timeout.as_secs(),
            })?
            .map_err(|e| McpError::Request {
                server: self.name.clone(),
                message: e.to_string(),
            })?;
        let mut parts: Vec<String> = Vec::new();
        for block in &result.content {
            let value = serde_json::to_value(block).unwrap_or(Value::Null);
            match value.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    value
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => parts.push("[unknown content omitted]".to_owned()),
            }
        }
        if parts.is_empty()
            && let Some(structured) = &result.structured_content
        {
            parts.push(structured.to_string());
        }
        Ok(CallOutcome {
            text: parts.join("\n"),
            is_error: result.is_error.unwrap_or(false),
        })
    }

    /// Close the connection: ask the server to stop, then kill the child if it lingers.
    pub async fn shutdown(&self) {
        let service = self.service.lock().await.take();
        if let Some(mut service) = service {
            // `close_with_timeout` cancels the service and runs transport cleanup; dropping the
            // child-process transport afterwards kills a process that did not exit.
            let _ = service.close_with_timeout(SHUTDOWN_TIMEOUT).await;
        }
    }
}

fn describe_json(value: &Value) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    };
    format!("got {kind}")
}
