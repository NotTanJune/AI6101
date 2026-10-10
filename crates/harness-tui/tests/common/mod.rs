#![allow(dead_code)]
//! Shared helpers: app construction, key messages, `TestBackend` drawing, an in-process MCP
//! server and a fully wired agent runtime over the mock model client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_agent::agent::{Agent, AgentConfig, AgentParts};
use harness_agent::approval::ApprovalPolicy;
use harness_agent::drift::SessionKb;
use harness_agent::mcp::{McpConnection, McpError};
use harness_agent::model::ModelClient;
use harness_agent::model::mock::{MockModelClient, Scripted};
use harness_agent::pricing::rate_card;
use harness_agent::registry::{Effort, ModelRegistry, ProviderId};
use harness_agent::tools::builtin::BuiltinTools;
use harness_agent::tools::{NamingMode, ToolRegistry};
use harness_agent::transcript::TranscriptWriter;
use harness_core::cost::CostLedger;
use harness_core::model::{Provenance, ToolCard, ToolKb};
use harness_core::redact::Redactor;
use harness_tui::{
    AgentHandle, AgentPort, AgentSetup, App, AppInit, ConnectFuture, Effect, McpLauncher,
    ModelInfo, Msg, UiCommand, UiEvent, ui,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn registry() -> ModelRegistry {
    ModelRegistry::load(repo_root().join("configs/models.toml")).expect("registry loads")
}

// ---------------------------------------------------------------------------------------------
// App and keys
// ---------------------------------------------------------------------------------------------

pub fn app_init(alias: &str, effort: Effort, servers: &[&str]) -> AppInit {
    AppInit {
        models: registry()
            .models()
            .iter()
            .map(ModelInfo::from_spec)
            .collect(),
        model_alias: alias.to_owned(),
        effort,
        budget_usd: 5.0,
        workspace: "/work/project".to_owned(),
        mcp_servers: servers.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// App on `luna` with maximum effort and no MCP servers.
pub fn fresh() -> App {
    App::new(app_init("luna", Effort::Max, &[]))
}

pub fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

pub fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

pub fn alt_enter() -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
}

pub fn shift_enter() -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
}

pub fn release(c: char) -> Msg {
    let mut k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    k.kind = KeyEventKind::Release;
    Msg::Key(k)
}

/// Type `text` key by key and return every effect produced.
pub fn type_text(app: &mut App, text: &str) -> Vec<Effect> {
    text.chars().flat_map(|c| app.update(ch(c))).collect()
}

/// Type `text` and press Enter.
pub fn say(app: &mut App, text: &str) -> Vec<Effect> {
    let mut effects = type_text(app, text);
    effects.extend(app.update(key(KeyCode::Enter)));
    effects
}

/// Commands among effects.
pub fn sent(effects: &[Effect]) -> Vec<UiCommand> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send(c) => Some(c.clone()),
            Effect::Quit => None,
        })
        .collect()
}

/// Advance the animation frame counter until every animation has finished (120 frames).
pub fn settle(app: &mut App) {
    for _ in 0..120 {
        app.update(Msg::Tick);
    }
}

pub fn ui_event(app: &mut App, event: UiEvent) -> Vec<Effect> {
    app.update(Msg::Ui(event))
}

pub fn agent_event(app: &mut App, event: harness_tui::AgentUiEvent) {
    app.update(Msg::Ui(UiEvent::Agent(event)));
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

pub fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| ui::render(frame, app)).expect("draw");
    terminal.backend().buffer().clone()
}

/// Buffer text, one line per row (trailing spaces trimmed).
pub fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Position of the first occurrence of `needle` within one row.
pub fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let area = buffer.area;
    for y in 0..area.height {
        let row: String = (0..area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect();
        if let Some(byte) = row.find(needle) {
            let x = row[..byte].chars().count();
            return Some((u16::try_from(x).expect("x"), y));
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// In-process MCP server
// ---------------------------------------------------------------------------------------------

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
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "first line",
        )]))
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

/// Launcher that hands out in-process servers; names in `fail` cannot be started.
#[derive(Debug, Clone, Default)]
pub struct TestLauncher {
    pub fail: Vec<String>,
}

impl McpLauncher for TestLauncher {
    fn connect<'a>(&'a self, name: &'a str) -> ConnectFuture<'a> {
        Box::pin(async move {
            if self.fail.iter().any(|f| f == name) {
                return Err(McpError::Spawn {
                    server: name.to_owned(),
                    message: "no such program".to_owned(),
                });
            }
            Ok(connect_test_server(name).await)
        })
    }
}

pub fn card(server: &str, name: &str, description: &str) -> ToolCard {
    ToolCard {
        lineage_id: format!("{server}::{name}"),
        server_id: server.to_owned(),
        tool_id: name.to_owned(),
        name: name.to_owned(),
        server_version: "kb-1".to_owned(),
        description: description.to_owned(),
        input_schema: serde_json::json!({"type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
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
// A fully wired runtime
// ---------------------------------------------------------------------------------------------

/// Where the mock model's replies come from.
pub enum Script {
    Replies(Vec<Scripted>),
    Dir(PathBuf),
}

pub struct SetupOpts {
    pub script: Script,
    pub alias: &'static str,
    pub effort: Effort,
    pub kb: Option<ToolKb>,
    pub servers: Vec<&'static str>,
    pub failing_servers: Vec<String>,
    pub cap_usd: f64,
    pub policy: ApprovalPolicy,
    pub transcript: bool,
}

impl SetupOpts {
    pub fn new(script: Script) -> Self {
        Self {
            script,
            alias: "luna",
            effort: Effort::Max,
            kb: None,
            servers: Vec::new(),
            failing_servers: Vec::new(),
            cap_usd: 5.0,
            policy: ApprovalPolicy::default(),
            transcript: true,
        }
    }
}

pub struct Fixture {
    pub alias: &'static str,
    pub servers: Vec<&'static str>,
    pub handle: AgentHandle,
    pub mock: Arc<MockModelClient>,
    pub ledger: Arc<Mutex<CostLedger>>,
    pub workspace: PathBuf,
    pub session_dir: PathBuf,
    pub dir: tempfile::TempDir,
}

impl Fixture {
    /// App state matching this runtime's starting model and servers.
    pub fn app(&self) -> App {
        App::new(app_init(self.alias, Effort::Max, &self.servers))
    }
}

/// Build the agent exactly as the CLI does and start its runtime thread.
pub fn spawn(opts: SetupOpts) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonical");
    let workspace = base.join("ws");
    let session_dir = base.join("session");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let models = registry();
    let mock = Arc::new(match opts.script {
        Script::Replies(replies) => MockModelClient::new(ProviderId::OpenAi, replies),
        Script::Dir(path) => {
            MockModelClient::from_fixture_dir(ProviderId::OpenAi, path).expect("fixture dir")
        }
    });
    let mut clients: HashMap<ProviderId, Arc<dyn ModelClient>> = HashMap::new();
    clients.insert(ProviderId::OpenAi, mock.clone());
    clients.insert(ProviderId::Gemini, mock.clone());
    let ledger = Arc::new(Mutex::new(CostLedger::new(
        opts.cap_usd,
        rate_card(&models),
    )));
    let builtins = BuiltinTools::new(&workspace, models.api_key_envs()).expect("builtin tools");
    let transcript = opts
        .transcript
        .then(|| TranscriptWriter::create(&session_dir, Redactor::default()).expect("transcript"));
    let agent = Agent::new(
        AgentConfig::new(opts.alias, opts.effort, "You are a test agent."),
        AgentParts {
            models: models.clone(),
            clients: clients.clone(),
            tools: ToolRegistry::new(NamingMode::Namespaced),
            builtins: Some(builtins),
            ledger: Arc::clone(&ledger),
            policy: opts.policy,
            transcript,
        },
    )
    .expect("agent");
    let session_kb = SessionKb::new(opts.kb.unwrap_or_else(|| kb(Vec::new())));
    let handle = AgentHandle::spawn(AgentSetup {
        agent,
        models,
        clients,
        session_kb,
        mcp_servers: opts.servers.iter().map(|s| (*s).to_owned()).collect(),
        launcher: Arc::new(TestLauncher {
            fail: opts.failing_servers,
        }),
    })
    .expect("spawn");
    Fixture {
        alias: opts.alias,
        servers: opts.servers,
        handle,
        mock,
        ledger,
        workspace,
        session_dir,
        dir,
    }
}

/// Poll `port` until `done` holds for the events collected so far. Panics on timeout and
/// prints what arrived.
pub fn wait_until(
    port: &mut dyn AgentPort,
    events: &mut Vec<UiEvent>,
    what: &str,
    done: impl Fn(&[UiEvent]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        events.extend(port.poll());
        if done(events) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; events so far:\n{events:#?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn has_turn_finished(events: &[UiEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            UiEvent::Agent(harness_tui::AgentUiEvent::TurnFinished { .. })
        )
    })
}
