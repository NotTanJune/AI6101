#![allow(dead_code)]
//! Shared helpers: a local HTTP stub server, registry access and request builders.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use harness_agent::model::{Item, ModelRequest, StreamEvent, ToolSchema};
use harness_agent::registry::{Effort, ModelRegistry, ModelSpec};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Dummy key used by tests. It must never appear in any output.
pub const DUMMY_KEY: &str = "sk-test-DUMMYKEY0123456789abcdef";

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn registry() -> ModelRegistry {
    ModelRegistry::load(repo_root().join("configs/models.toml")).expect("registry loads")
}

pub fn spec(alias: &str) -> ModelSpec {
    registry().get(alias).expect("alias exists").clone()
}

pub fn tool_schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

pub fn request<'a>(
    spec: &'a ModelSpec,
    effort: Effort,
    history: &'a [Item],
    tools: &'a [ToolSchema],
) -> ModelRequest<'a> {
    ModelRequest {
        spec,
        effort,
        instructions: "be helpful",
        history,
        tools,
        max_output_tokens: 4096,
        json_schema: None,
    }
}

pub fn drain(mut rx: UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

pub fn stream_channel() -> (
    tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    UnboundedReceiver<StreamEvent>,
) {
    unbounded_channel()
}

/// One canned HTTP reply.
#[derive(Debug, Clone)]
pub struct StubReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl StubReply {
    pub fn json(body: &Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }

    pub fn sse(events: &[Value]) -> Self {
        let mut body = String::new();
        for event in events {
            let kind = event["type"].as_str().unwrap_or("message");
            body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
        }
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }

    pub fn status(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.to_owned(),
        }
    }
}

/// A request captured by the stub server.
#[derive(Debug, Clone)]
pub struct Captured {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Local HTTP server serving canned replies in order. Binds to loopback only.
pub struct StubServer {
    pub base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl StubServer {
    pub async fn start(replies: Vec<StubReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_task = Arc::clone(&captured);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let queue = Arc::clone(&queue);
                let captured = Arc::clone(&captured_task);
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    captured.lock().expect("lock").push(request);
                    let reply = queue.lock().expect("lock").pop_front().unwrap_or_else(|| {
                        StubReply::status(500, &[], "{\"error\":{\"message\":\"stub exhausted\"}}")
                    });
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (k, v) in &reply.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(reply.body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            captured,
        }
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.captured.lock().expect("lock").clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Captured> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let path = request_line.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = serde_json::from_slice(&buf[header_end..header_end + length]).unwrap_or(Value::Null);
    Some(Captured {
        path,
        headers,
        body,
    })
}

// ---------------------------------------------------------------------------------------------
// In-process MCP server
// ---------------------------------------------------------------------------------------------

use std::time::Duration;

use harness_agent::mcp::McpConnection;
use harness_core::model::{Provenance, ToolCard, ToolKb};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddArgs {
    pub a: i32,
    pub b: i32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShoutArgs {
    pub text: String,
}

/// Test MCP server with the tools `add`, `shout`, `mixed` and `boom`.
#[derive(Debug, Clone)]
pub struct TestMcpServer {
    tool_router: ToolRouter<Self>,
}

impl Default for TestMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router(router = tool_router)]
impl TestMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(name = "add", description = "LIVE add description")]
    pub fn add(&self, Parameters(AddArgs { a, b }): Parameters<AddArgs>) -> String {
        (a + b).to_string()
    }

    #[tool(name = "shout", description = "LIVE shout description")]
    pub fn shout(&self, Parameters(ShoutArgs { text }): Parameters<ShoutArgs>) -> String {
        text.to_uppercase()
    }

    #[tool(name = "mixed", description = "Returns text and an image")]
    pub fn mixed(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![
            ContentBlock::text("first line"),
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::text("last line"),
        ]))
    }

    #[tool(name = "boom", description = "Always fails")]
    pub fn boom(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text("it broke")]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TestMcpServer {}

/// Connect a client to a fresh in-process [`TestMcpServer`] over `tokio::io::duplex`.
pub async fn connect_test_server(name: &str) -> McpConnection {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(server) = TestMcpServer::new().serve(server_io).await {
            let _ = server.waiting().await;
        }
    });
    McpConnection::connect_transport(name, client_io, Duration::from_secs(10))
        .await
        .expect("connect in-process server")
}

pub fn card(server: &str, name: &str, description: &str) -> ToolCard {
    ToolCard {
        lineage_id: format!("{server}::{name}"),
        server_id: server.to_owned(),
        tool_id: name.to_owned(),
        name: name.to_owned(),
        server_version: "kb-1".to_owned(),
        description: description.to_owned(),
        input_schema: json!({"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                             "required": ["a", "b"]}),
        output_schema: None,
        usage_rules: vec![],
        provenance: Provenance {
            release_id: "rel".to_owned(),
            source_file: "catalog.json".to_owned(),
            source_pointer: None,
        },
    }
}

pub fn kb(cards: Vec<ToolCard>) -> ToolKb {
    ToolKb {
        schema_version: "tool-kb/1".to_owned(),
        kb_id: "kb-test".to_owned(),
        release_id: "rel".to_owned(),
        server_version: "kb-1".to_owned(),
        cards,
    }
}

// ---------------------------------------------------------------------------------------------
// Agent harness
// ---------------------------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use harness_agent::agent::{Agent, AgentConfig, AgentParts, StopPolicy, StopReason};
use harness_agent::approval::{ApprovalPolicy, ApprovalRequest, Decision};
use harness_agent::drift::DriftReport;
use harness_agent::events::AgentEvent;
use harness_agent::model::mock::{MockModelClient, Scripted};
use harness_agent::model::{ModelClient, TokenUsage};
use harness_agent::pricing::rate_card;
use harness_agent::registry::ProviderId;
use harness_agent::tools::builtin::BuiltinTools;
use harness_agent::tools::{NamingMode, ToolRegistry};
use harness_agent::transcript::TranscriptWriter;
use harness_core::cost::CostLedger;
use harness_core::redact::Redactor;
use tokio_util::sync::CancellationToken;

/// Simplified, clonable view of an [`AgentEvent`].
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    TurnStarted,
    Text(String),
    ToolStarted(String),
    ToolFinished { output: String, is_error: bool },
    Approval(ApprovalRequest),
    Usage { alias: String, cost: Option<f64> },
    Drift(DriftReport),
    Notice(String),
    Error(String),
    Finished(StopReason),
}

pub struct Harness {
    pub agent: Agent,
    pub mock: Arc<MockModelClient>,
    pub ledger: Arc<StdMutex<CostLedger>>,
    pub root: PathBuf,
    pub session_dir: PathBuf,
    pub dir: tempfile::TempDir,
}

pub struct HarnessOpts {
    pub script: Vec<Scripted>,
    pub policy: ApprovalPolicy,
    pub cap_usd: f64,
    pub alias: &'static str,
    pub effort: Effort,
    pub stop_policy: StopPolicy,
    pub max_steps: u32,
    pub max_output_tokens: u32,
    pub tool_result_max_chars: Option<usize>,
    pub transcript: bool,
    pub redact_key: bool,
}

impl HarnessOpts {
    pub fn new(script: Vec<Scripted>) -> Self {
        Self {
            script,
            policy: ApprovalPolicy::default(),
            cap_usd: 1000.0,
            alias: "sol",
            effort: Effort::High,
            stop_policy: StopPolicy::Interactive,
            max_steps: 20,
            max_output_tokens: 1000,
            tool_result_max_chars: None,
            transcript: false,
            redact_key: false,
        }
    }
}

pub fn harness(opts: HarnessOpts) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonical");
    let root = base.join("ws");
    let session_dir = base.join("session");
    std::fs::create_dir_all(&root).expect("ws");
    let models = registry();
    let ledger = Arc::new(StdMutex::new(CostLedger::new(
        opts.cap_usd,
        rate_card(&models),
    )));
    let mock = Arc::new(MockModelClient::new(ProviderId::OpenAi, opts.script));
    let mut clients: HashMap<ProviderId, Arc<dyn ModelClient>> = HashMap::new();
    clients.insert(ProviderId::OpenAi, mock.clone());
    clients.insert(ProviderId::Gemini, mock.clone());
    // An empty scrub list on purpose: the agent itself must add the registry's API key variables.
    let builtins = BuiltinTools::new(&root, Vec::<String>::new()).expect("builtin tools");
    let redactor = if opts.redact_key {
        Redactor::new(vec![DUMMY_KEY.to_owned()])
    } else {
        Redactor::default()
    };
    let transcript = opts
        .transcript
        .then(|| TranscriptWriter::create(&session_dir, redactor).expect("transcript"));
    let mut config = AgentConfig::new(opts.alias, opts.effort, "You are a test agent.");
    config.stop_policy = opts.stop_policy;
    config.max_steps = opts.max_steps;
    config.max_output_tokens = opts.max_output_tokens;
    config.tool_result_max_chars = opts.tool_result_max_chars;
    let agent = Agent::new(
        config,
        AgentParts {
            models,
            clients,
            tools: ToolRegistry::new(NamingMode::Namespaced),
            builtins: Some(builtins),
            ledger: Arc::clone(&ledger),
            policy: opts.policy,
            transcript,
        },
    )
    .expect("agent");
    Harness {
        agent,
        mock,
        ledger,
        root,
        session_dir,
        dir,
    }
}

/// Run one turn, collecting events. `answer` decides every approval request.
pub async fn run_turn(
    agent: &mut Agent,
    text: &str,
    cancel: CancellationToken,
    mut answer: impl FnMut(&ApprovalRequest) -> Decision,
) -> (StopReason, Vec<Seen>) {
    let (tx, mut rx) = unbounded_channel();
    let run = agent.run_turn(text, tx, cancel);
    let collect = async {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(match event {
                AgentEvent::TurnStarted => Seen::TurnStarted,
                AgentEvent::TextDelta(t) => Seen::Text(t),
                AgentEvent::ToolCallStarted { name, .. } => Seen::ToolStarted(name),
                AgentEvent::ToolCallFinished {
                    output, is_error, ..
                } => Seen::ToolFinished { output, is_error },
                AgentEvent::ApprovalRequested(request, reply) => {
                    let decision = answer(&request);
                    let _ = reply.send(decision);
                    Seen::Approval(request)
                }
                AgentEvent::Usage {
                    model_alias,
                    cost_usd,
                    ..
                } => Seen::Usage {
                    alias: model_alias,
                    cost: cost_usd,
                },
                AgentEvent::Drift(report) => Seen::Drift(report),
                AgentEvent::Notice(n) => Seen::Notice(n),
                AgentEvent::Error(e) => Seen::Error(e),
                AgentEvent::TurnFinished { reason } => Seen::Finished(reason),
            });
        }
        seen
    };
    tokio::join!(run, collect)
}

pub fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input: Some(input),
        output_including_reasoning: Some(output),
        ..TokenUsage::default()
    }
}
