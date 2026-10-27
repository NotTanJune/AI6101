//! Real in-process MCP round trips for isolated live canary execution.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{card, kb};
use harness_agent::live_canary::LiveCanaryExecutor;
use harness_agent::mcp::{McpCanaryConfig, McpConnection};
use harness_core::canary::{CanaryGate, CanaryReport, Probe, ProbeExecutor};
use harness_core::events::{EventEmitter, EventSource, VecSink};
use harness_core::{CanaryStatus, FixedClock, Redactor, Strategy, ToolKb};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct CanaryServer {
    tool_router: ToolRouter<Self>,
    calls: Arc<Mutex<Vec<String>>>,
    reset_error: bool,
}

#[tool_router(router = tool_router)]
impl CanaryServer {
    fn new(calls: Arc<Mutex<Vec<String>>>, reset_error: bool) -> Self {
        Self {
            tool_router: Self::tool_router(),
            calls,
            reset_error,
        }
    }

    #[tool(name = "reset", description = "Restore the disposable baseline")]
    fn reset(&self) -> Result<CallToolResult, ErrorData> {
        self.calls.lock().unwrap().push("reset".into());
        if self.reset_error {
            Ok(CallToolResult::error(vec![ContentBlock::text(
                "reset refused",
            )]))
        } else {
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "reset done",
            )]))
        }
    }

    #[tool(name = "accepted", description = "Accept a probe")]
    fn accepted(&self) -> String {
        self.calls.lock().unwrap().push("accepted".into());
        "accepted execution".into()
    }

    #[tool(name = "rejected", description = "Return a server-declared error")]
    fn rejected(&self) -> Result<CallToolResult, ErrorData> {
        self.calls.lock().unwrap().push("rejected".into());
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "probe refused",
        )]))
    }

    #[tool(name = "slow", description = "Exceed the canary timeout")]
    async fn slow(&self) -> String {
        self.calls.lock().unwrap().push("slow".into());
        tokio::time::sleep(Duration::from_millis(250)).await;
        "late".into()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CanaryServer {}

async fn connect(
    reset_error: bool,
    timeout: Duration,
) -> (Arc<McpConnection>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = CanaryServer::new(calls.clone(), reset_error);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_io).await {
            let _ = service.waiting().await;
        }
    });
    let connection = McpConnection::connect_transport("disposable", client_io, timeout)
        .await
        .expect("connect real MCP test transport");
    (Arc::new(connection), calls)
}

async fn target(connection: &McpConnection) -> ToolKb {
    kb(connection
        .list_tools()
        .await
        .expect("live listing")
        .iter()
        .map(|tool| {
            let mut card = card("disposable", &tool.name, &tool.description);
            card.input_schema = tool.input_schema.clone();
            card
        })
        .collect())
}

fn config() -> McpCanaryConfig {
    McpCanaryConfig {
        isolated: true,
        reset_tool: "reset".into(),
        reset_arguments: json!({}),
    }
}

fn probe(tool: &str, control: bool) -> Probe {
    Probe {
        probe_id: format!("probe-{tool}"),
        tool_id: tool.into(),
        lineage_id: format!("disposable::{tool}"),
        arguments: Default::default(),
        expected: "MCP accepts execution; task completion is not checked".into(),
        is_control: control,
        mutating: true,
    }
}

async fn run(mut executor: LiveCanaryExecutor, probes: Vec<Probe>) -> CanaryReport {
    tokio::task::spawn_blocking(move || {
        let mut emitter = EventEmitter::new(
            "live-canary-test",
            EventSource::Rust,
            Arc::new(FixedClock::default()),
            Redactor::default(),
            Box::new(VecSink::new()),
        );
        CanaryGate::run_probes(&probes, &mut executor, &mut emitter, Strategy::FullRewrite)
            .expect("emit canary results")
    })
    .await
    .expect("blocking canary task")
}

#[tokio::test]
async fn live_canary_resets_before_each_actual_mcp_probe() {
    let (connection, calls) = connect(false, Duration::from_secs(1)).await;
    let target = target(&connection).await;
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let report = run(
        executor,
        vec![probe("accepted", false), probe("accepted", true)],
    )
    .await;
    assert_eq!(report.status, CanaryStatus::Pass);
    assert_eq!(
        *calls.lock().unwrap(),
        ["reset", "accepted", "reset", "accepted"]
    );
    assert!(
        report
            .results
            .iter()
            .all(|result| result.executed && result.action_seq.is_some())
    );
    assert!(
        report.results[0]
            .actual
            .as_ref()
            .unwrap()
            .contains("task completion not verified")
    );
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_server_declared_error_fails_the_probe() {
    let (connection, calls) = connect(false, Duration::from_secs(1)).await;
    let target = target(&connection).await;
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let report = run(executor, vec![probe("rejected", false)]).await;
    assert_eq!(report.status, CanaryStatus::Fail);
    assert!(
        report.results[0]
            .actual
            .as_ref()
            .unwrap()
            .contains("probe refused")
    );
    assert_eq!(*calls.lock().unwrap(), ["reset", "rejected"]);
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_timeout_is_inconclusive_and_never_passes() {
    let (connection, calls) = connect(false, Duration::from_millis(50)).await;
    let target = target(&connection).await;
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let report = run(
        executor,
        vec![probe("slow", false), probe("accepted", true)],
    )
    .await;
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.promote);
    assert!(
        !report.results[1].executed,
        "an unresolved timeout disables subsequent probes"
    );
    assert!(
        report.results[0]
            .actual
            .as_ref()
            .unwrap()
            .contains("timed out")
    );
    assert_eq!(*calls.lock().unwrap(), ["reset", "slow"]);
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_reset_failure_prevents_probe_execution() {
    let (connection, calls) = connect(true, Duration::from_secs(1)).await;
    let target = target(&connection).await;
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let report = run(executor, vec![probe("accepted", false)]).await;
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.results[0].executed);
    assert!(report.results[0].action_seq.is_none());
    assert_eq!(*calls.lock().unwrap(), ["reset"]);
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_missing_or_invalid_permission_executes_no_mcp_calls() {
    let (connection, calls) = connect(false, Duration::from_secs(1)).await;
    let target = target(&connection).await;
    let invalid_configs = [
        None,
        Some(McpCanaryConfig {
            isolated: false,
            ..config()
        }),
        Some(McpCanaryConfig {
            reset_tool: String::new(),
            ..config()
        }),
        Some(McpCanaryConfig {
            reset_tool: "absent".into(),
            ..config()
        }),
        Some(McpCanaryConfig {
            reset_arguments: json!([]),
            ..config()
        }),
    ];
    for config in invalid_configs {
        let executor = LiveCanaryExecutor::new(
            "disposable",
            connection.clone(),
            &target,
            config.as_ref(),
            tokio::runtime::Handle::current(),
        );
        assert!(!executor.is_isolated());
        let report = run(executor, vec![probe("accepted", false)]).await;
        assert_eq!(report.status, CanaryStatus::Inconclusive);
        assert!(report.results.iter().all(|result| !result.executed));
    }
    assert!(calls.lock().unwrap().is_empty());
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_refuses_cross_server_identity_and_unknown_tool_pairs() {
    let (connection, calls) = connect(false, Duration::from_secs(1)).await;
    let mut target = target(&connection).await;
    for server in ["another", "disposable"] {
        if server == "disposable" {
            target.cards[0].server_id = "another".into();
        }
        let executor = LiveCanaryExecutor::new(
            server,
            connection.clone(),
            &target,
            Some(&config()),
            tokio::runtime::Handle::current(),
        );
        assert!(!executor.is_isolated());
        assert_eq!(
            run(executor, vec![probe("accepted", false)]).await.status,
            CanaryStatus::Inconclusive
        );
    }
    assert!(calls.lock().unwrap().is_empty());
    let target = self::target(&connection).await;
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let mut wrong_lineage = probe("accepted", false);
    wrong_lineage.lineage_id = "another::accepted".into();
    let report = run(executor, vec![wrong_lineage, probe("absent", false)]).await;
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert_eq!(*calls.lock().unwrap(), ["reset", "reset"]);
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_broken_transport_never_counts_as_accepted_execution() {
    // A disposable relay lets the test sever the actual wire after a real reset,
    // without replacing the MCP connection or injecting a scripted executor.
    let (server_io, mut relay_server) = tokio::io::duplex(64 * 1024);
    let (client_io, mut relay_client) = tokio::io::duplex(64 * 1024);
    let relay = tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut relay_server, &mut relay_client).await;
    });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = CanaryServer::new(calls.clone(), false);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_io).await {
            let _ = service.waiting().await;
        }
    });
    let connection = Arc::new(
        McpConnection::connect_transport("disposable", client_io, Duration::from_secs(1))
            .await
            .expect("connect over severable in-process wire"),
    );
    let target = target(&connection).await;
    let mut executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    );
    let executor = tokio::task::spawn_blocking(move || {
        executor.reset().expect("real successful reset");
        executor
    })
    .await
    .unwrap();
    relay.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    tokio::task::spawn_blocking(move || {
        let mut executor = executor;
        let error = executor
            .execute(&probe("accepted", false))
            .expect_err("broken transport");
        assert!(error.to_string().contains("request failed"));
        assert!(
            executor.execute(&probe("accepted", false)).is_err(),
            "a reset is single use"
        );
    })
    .await
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), ["reset"]);
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_pre_cancelled_session_makes_no_mcp_calls() {
    let (connection, calls) = connect(false, Duration::from_secs(1)).await;
    let target = target(&connection).await;
    let token = CancellationToken::new();
    token.cancel();
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    )
    .with_cancellation(token);
    assert!(!executor.is_isolated());
    let report = run(executor, vec![probe("accepted", false)]).await;
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.results[0].executed);
    assert!(calls.lock().unwrap().is_empty());
    connection.shutdown().await;
}

#[tokio::test]
async fn live_canary_mid_call_cancellation_stops_waiting_and_following_control() {
    let (connection, calls) = connect(false, Duration::from_secs(2)).await;
    let target = target(&connection).await;
    let token = CancellationToken::new();
    let executor = LiveCanaryExecutor::new(
        "disposable",
        connection.clone(),
        &target,
        Some(&config()),
        tokio::runtime::Handle::current(),
    )
    .with_cancellation(token.clone());
    let task = tokio::spawn(run(
        executor,
        vec![probe("slow", false), probe("accepted", true)],
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if calls.lock().unwrap().iter().any(|call| call == "slow") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server received the real slow probe");
    token.cancel();
    let report = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancelled canary stops waiting")
        .expect("canary task");
    assert_eq!(report.status, CanaryStatus::Inconclusive);
    assert!(!report.promote);
    assert!(
        report.results[0]
            .actual
            .as_ref()
            .unwrap()
            .contains("live canary cancelled")
    );
    assert!(!report.results[1].executed);
    assert_eq!(*calls.lock().unwrap(), ["reset", "slow"]);
    connection.shutdown().await;
}
