//! MCP client and tool registry behaviour using an in-process rmcp server.

mod common;

use common::{card, connect_test_server, kb};
use harness_agent::mcp::{LiveTool, McpConfig, McpConnection, McpError, McpServerConfig};
use harness_agent::tools::{
    BuiltinKind, NamingMode, ToolRegistry, ToolSource, ToolSpec, ToolsError, namespaced_name,
};
use serde_json::json;

#[tokio::test]
async fn mcp_lists_tools_and_calls_them() {
    let conn = connect_test_server("calc").await;
    let tools = conn.list_tools().await.expect("list");
    let mut names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["add", "boom", "mixed", "shout"]);
    let add = tools.iter().find(|t| t.name == "add").expect("add");
    assert_eq!(add.description, "LIVE add description");
    assert_eq!(add.input_schema["type"], "object");
    assert_eq!(add.input_schema["properties"]["a"]["type"], "integer");

    let sum = conn
        .call_tool("add", &json!({"a": 2, "b": 40}))
        .await
        .expect("call");
    assert_eq!(sum.text, "42");
    assert!(!sum.is_error);

    let shout = conn
        .call_tool("shout", &json!({"text": "hi there"}))
        .await
        .expect("call");
    assert_eq!(shout.text, "HI THERE");

    conn.shutdown().await;
    let err = conn.list_tools().await.expect_err("closed");
    assert!(matches!(err, McpError::Closed(_)), "{err}");
}

#[tokio::test]
async fn mcp_non_text_parts_are_described_and_errors_flagged() {
    let conn = connect_test_server("calc").await;
    let mixed = conn.call_tool("mixed", &json!(null)).await.expect("mixed");
    assert_eq!(mixed.text, "first line\n[image content omitted]\nlast line");
    assert!(!mixed.is_error);

    let boom = conn.call_tool("boom", &json!({})).await.expect("boom");
    assert!(boom.is_error);
    assert_eq!(boom.text, "it broke");

    // Bad arguments surface as a request error or an error result, never a panic.
    let bad = conn.call_tool("add", &json!({"a": "x"})).await;
    match bad {
        Ok(outcome) => assert!(outcome.is_error, "{outcome:?}"),
        Err(McpError::Request { .. }) => {}
        Err(other) => panic!("unexpected {other}"),
    }
    let not_object = conn
        .call_tool("add", &json!([1, 2]))
        .await
        .expect_err("array");
    assert!(
        matches!(not_object, McpError::InvalidArguments(_)),
        "{not_object}"
    );
    conn.shutdown().await;
}

#[test]
fn mcp_config_parses_standard_shape_and_validates() {
    let text = r#"{"mcpServers": {
        "files": {"command": "npx", "args": ["-y", "server-files"], "env": {"TOKEN": "super-secret-value"}},
        "remote": {"url": "https://example.com/mcp"}
    }}"#;
    let config = McpConfig::from_json_str(text).expect("parse");
    assert_eq!(config.servers.len(), 2);
    let files = &config.servers["files"];
    assert_eq!(files.command.as_deref(), Some("npx"));
    assert_eq!(files.args, vec!["-y", "server-files"]);
    assert_eq!(files.env["TOKEN"], "super-secret-value");
    assert_eq!(
        config.servers["remote"].url.as_deref(),
        Some("https://example.com/mcp")
    );
    // Env values never reach Debug output.
    assert!(!format!("{config:?}").contains("super-secret-value"));
    assert!(format!("{files:?}").contains("TOKEN"));

    assert!(McpConfig::from_json_str(r#"{"mcpServers": {"x": {}}}"#).is_err());
    assert!(
        McpConfig::from_json_str(r#"{"mcpServers": {"x": {"command": "a", "url": "b"}}}"#).is_err()
    );
    assert!(McpConfig::from_json_str("not json").is_err());
}

#[tokio::test]
async fn mcp_stdio_spawn_failure_is_a_clean_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = McpServerConfig {
        command: Some("/definitely/not/a/real/binary".into()),
        ..McpServerConfig::default()
    };
    let err = McpConnection::connect_stdio(
        "ghost",
        &cfg,
        &dir.path().join("logs/ghost.stderr.log"),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect_err("spawn fails");
    assert!(matches!(err, McpError::Spawn { .. }), "{err}");
}

/// Minimal newline-delimited JSON-RPC MCP server for the stdio test. Needs `python3`.
const FAKE_SERVER_PY: &str = r#"
import json, sys
sys.stderr.write("fake-server-started\n")
sys.stderr.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "0.0.1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo input",
                             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        text = msg["params"].get("arguments", {}).get("text", "")
        result = {"content": [{"type": "text", "text": "echo:" + text}]}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#;

#[tokio::test]
async fn mcp_stdio_child_process_round_trip_with_stderr_in_a_log_file() {
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output();
    if python.is_err() {
        eprintln!("python3 not available; skipping stdio round trip");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake_server.py");
    std::fs::write(&script, FAKE_SERVER_PY).expect("write script");
    let cfg = McpServerConfig {
        command: Some("python3".into()),
        args: vec![script.display().to_string()],
        ..McpServerConfig::default()
    };
    let log = dir.path().join("logs").join("fake.stderr.log");
    let conn = McpConnection::connect_stdio("fake", &cfg, &log, std::time::Duration::from_secs(10))
        .await
        .expect("connect to child process");
    let tools = conn.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let out = conn
        .call_tool("echo", &json!({"text": "ping"}))
        .await
        .expect("call");
    assert_eq!(out.text, "echo:ping");
    conn.shutdown().await;
    let logged = std::fs::read_to_string(&log).expect("stderr log");
    assert!(logged.contains("fake-server-started"), "{logged:?}");
}

mod tools {
    use super::*;

    fn live(name: &str, description: &str) -> LiveTool {
        LiveTool {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn registry_exposes_kb_description_even_when_live_differs() {
        let conn = connect_test_server("calc").await;
        let live_tools = conn.list_tools().await.expect("list");
        let live_add = live_tools.iter().find(|t| t.name == "add").expect("add");
        assert_eq!(live_add.description, "LIVE add description");

        let kb = kb(vec![
            card("calc", "add", "KB add description"),
            card("calc", "legacy", "KB only tool"),
            card("other", "unrelated", "belongs to another server"),
        ]);
        let mut registry = ToolRegistry::new(NamingMode::Namespaced);
        let report = registry
            .add_mcp_from_kb("calc", &kb, &live_tools)
            .expect("expose");

        assert_eq!(
            report.exposed,
            vec!["mcp__calc__add".to_owned(), "mcp__calc__legacy".to_owned()]
        );
        assert_eq!(report.kb_only, vec!["mcp__calc__legacy".to_owned()]);
        let mut live_only = report.live_only.clone();
        live_only.sort();
        assert_eq!(live_only, vec!["boom", "mixed", "shout"]);

        let spec = registry.get("mcp__calc__add").expect("add exposed");
        assert_eq!(spec.description, "KB add description");
        assert_eq!(spec.parameters, kb.cards[0].input_schema);
        assert_eq!(
            spec.source,
            ToolSource::Mcp {
                server: "calc".into(),
                tool: "add".into()
            }
        );
        // Live-only tools and other servers' cards are not exposed.
        assert!(registry.get("mcp__calc__shout").is_none());
        assert!(registry.get("mcp__other__unrelated").is_none());

        let schemas = registry.to_schemas();
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].name, "mcp__calc__add");
        assert_eq!(schemas[0].description, "KB add description");
        conn.shutdown().await;
    }

    #[test]
    fn bare_mode_and_collisions() {
        let kb_a = kb(vec![card("a", "search", "A search")]);
        let kb_b = kb(vec![card("b", "search", "B search")]);
        let mut registry = ToolRegistry::new(NamingMode::Bare);
        registry
            .add_mcp_from_kb("a", &kb_a, &[live("search", "")])
            .expect("first");
        assert!(registry.get("search").is_some());
        let err = registry
            .add_mcp_from_kb("b", &kb_b, &[live("search", "")])
            .expect_err("cross-server collision");
        assert_eq!(
            err,
            ToolsError::NameCollision {
                name: "search".into()
            }
        );
        // Nothing from the failed add is left behind.
        assert_eq!(registry.specs().len(), 1);

        // Namespaced mode keeps the two apart.
        let mut namespaced = ToolRegistry::new(NamingMode::Namespaced);
        namespaced.add_mcp_from_kb("a", &kb_a, &[]).expect("a");
        namespaced.add_mcp_from_kb("b", &kb_b, &[]).expect("b");
        assert!(namespaced.get("mcp__a__search").is_some());
        assert!(namespaced.get("mcp__b__search").is_some());

        // A namespaced collision after sanitising is an error.
        let mut clash = ToolRegistry::new(NamingMode::Namespaced);
        let kb_dot = kb(vec![card("s", "a.b", "x"), card("s", "a b", "y")]);
        let err = clash
            .add_mcp_from_kb("s", &kb_dot, &[])
            .expect_err("sanitised clash");
        assert_eq!(
            err,
            ToolsError::NameCollision {
                name: "mcp__s__a_b".into()
            }
        );

        registry.remove_mcp_server("a");
        assert!(registry.is_empty());
    }

    #[test]
    fn namespaced_names_are_sanitised_and_truncated_with_a_hash() {
        assert_eq!(
            namespaced_name("my server", "do.it!"),
            "mcp__my_server__do_it_"
        );
        let long_tool = "x".repeat(100);
        let name = namespaced_name("srv", &long_tool);
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("mcp__srv__xxxx"));
        // Deterministic, and different long names get different suffixes.
        assert_eq!(name, namespaced_name("srv", &long_tool));
        let other = namespaced_name("srv", &format!("{}y", "x".repeat(99)));
        assert_eq!(other.len(), 64);
        assert_ne!(name, other);
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn registry_rejects_duplicate_add() {
        let mut registry = ToolRegistry::new(NamingMode::Bare);
        let spec = ToolSpec {
            exposed_name: "read_file".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            source: ToolSource::Builtin(BuiltinKind::ReadFile),
        };
        registry.add(spec.clone()).expect("first");
        assert!(registry.add(spec).is_err());
    }
}
