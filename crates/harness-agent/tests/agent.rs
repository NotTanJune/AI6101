//! Agent loop behaviour with the mock model client and an in-process MCP server.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{HarnessOpts, Seen, card, connect_test_server, harness, kb, run_turn, usage};
use harness_agent::agent::{AgentError, StopPolicy, StopReason};
use harness_agent::approval::{ApprovalClass, ApprovalPolicy, DENIED_TEXT, Decision, PolicyAction};
use harness_agent::model::mock::Scripted;
use harness_agent::model::{Item, ModelError};
use harness_agent::registry::Effort;
use harness_core::CostStatus;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn tool_results(history: &[Item]) -> Vec<(String, String, bool)> {
    history
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult {
                name,
                output,
                is_error,
                ..
            } => Some((name.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn no_prompt(_: &harness_agent::approval::ApprovalRequest) -> Decision {
    panic!("no approval prompt expected");
}

#[tokio::test]
async fn agent_two_step_tool_loop_ends_with_text() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls("let me look", &[("read_file", json!({"path": "a.txt"}))]),
        Scripted::text("the file says hello"),
    ]));
    std::fs::write(h.root.join("a.txt"), "hello\nworld\n").expect("write");

    let (reason, seen) = run_turn(
        &mut h.agent,
        "what is in a.txt?",
        CancellationToken::new(),
        no_prompt,
    )
    .await;
    assert_eq!(reason, StopReason::Completed);

    let kinds: Vec<&str> = seen
        .iter()
        .map(|s| match s {
            Seen::TurnStarted => "start",
            Seen::Text(_) => "text",
            Seen::ToolStarted(_) => "tool_start",
            Seen::ToolFinished { .. } => "tool_end",
            Seen::Usage { .. } => "usage",
            Seen::Finished(_) => "finish",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "text",
            "usage",
            "tool_start",
            "tool_end",
            "text",
            "usage",
            "finish"
        ]
    );
    assert_eq!(seen.last(), Some(&Seen::Finished(StopReason::Completed)));
    assert!(matches!(&seen[1], Seen::Text(t) if t == "let me look"));
    assert!(matches!(&seen[5], Seen::Text(t) if t == "the file says hello"));
    // Usage events carry the cost computed from the rate card: (100*2 + 20*10)/1e6.
    assert!(matches!(&seen[2], Seen::Usage { alias, cost: Some(c) }
        if alias == "sol" && (c - 0.0004).abs() < 1e-12));

    let history = h.agent.history();
    assert_eq!(history.len(), 4);
    assert!(matches!(&history[0], Item::User { text } if text == "what is in a.txt?"));
    assert!(matches!(&history[1], Item::Assistant(t) if t.tool_calls.len() == 1));
    assert_eq!(
        tool_results(history),
        vec![(
            "read_file".to_owned(),
            "     1\thello\n     2\tworld\n".to_owned(),
            false
        )]
    );
    assert!(matches!(&history[3], Item::Assistant(t) if t.text == "the file says hello"));

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].history.len(), 1);
    assert_eq!(requests[1].history.len(), 3, "user, assistant, tool result");
    assert_eq!(requests[0].model_alias, "sol");
    assert_eq!(requests[0].effort, Effort::High);
    assert_eq!(requests[0].instructions, "You are a test agent.");
    assert_eq!(
        requests[0].tool_names,
        vec![
            "read_file",
            "list_dir",
            "grep",
            "write_file",
            "edit_file",
            "run_shell"
        ]
    );

    // Both calls were settled on the ledger under the agent phase.
    let ledger = h.ledger.lock().expect("lock");
    assert_eq!(ledger.records().len(), 2);
    assert!((ledger.spent_usd() - 0.0008).abs() < 1e-12);
    assert!(ledger.outstanding_reserved_usd().abs() < 1e-12);
}

#[tokio::test]
async fn agent_calls_mcp_tools_through_the_kb_exposed_name() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("mcp__calc__add", json!({"a": 2, "b": 40}))]),
        Scripted::text("42"),
    ]));
    let conn = Arc::new(connect_test_server("calc").await);
    let live = conn.list_tools().await.expect("list");
    let kb = kb(vec![card("calc", "add", "KB add description")]);
    h.agent
        .tools_mut()
        .add_mcp_from_kb("calc", &kb, &live)
        .expect("expose");
    h.agent.add_mcp_connection("calc", Arc::clone(&conn));

    let (reason, _) = run_turn(&mut h.agent, "add", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(
        tool_results(h.agent.history()),
        vec![("mcp__calc__add".to_owned(), "42".to_owned(), false)]
    );
    let request = &h.mock.requests()[0];
    assert!(request.tool_names.contains(&"mcp__calc__add".to_owned()));
    assert!(!request.tool_names.contains(&"mcp__calc__shout".to_owned()));
    conn.shutdown().await;

    // An MCP server that is not connected is a tool error, not a crash.
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("mcp__calc__add", json!({"a": 1, "b": 1}))]),
        Scripted::text("ok"),
    ]));
    h.agent
        .tools_mut()
        .add_mcp_from_kb("calc", &kb_of_add(), &[])
        .expect("expose");
    let (reason, _) = run_turn(&mut h.agent, "add", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    let results = tool_results(h.agent.history());
    assert!(
        results[0].2 && results[0].1.contains("not connected"),
        "{results:?}"
    );
}

fn kb_of_add() -> harness_core::model::ToolKb {
    kb(vec![card("calc", "add", "KB add description")])
}

#[tokio::test]
async fn agent_write_tool_waits_for_approval_and_proceeds_on_once() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[(
                "write_file",
                json!({"path": "out.txt", "content": "hello\n"}),
            )],
        ),
        Scripted::text("written"),
    ]));
    let target = h.root.join("out.txt");
    let mut asked = 0;
    let (reason, seen) = run_turn(
        &mut h.agent,
        "write it",
        CancellationToken::new(),
        |request| {
            asked += 1;
            assert_eq!(request.class, ApprovalClass::Write);
            assert_eq!(request.tool, "write_file");
            assert_eq!(request.summary, "Create out.txt (6 bytes)");
            assert!(
                request
                    .diff
                    .as_deref()
                    .is_some_and(|d| d.contains("+hello"))
            );
            assert!(!target.exists(), "nothing is written before the answer");
            Decision::Once
        },
    )
    .await;
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(asked, 1);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
    assert!(seen.iter().any(|s| matches!(s, Seen::Approval(_))));
    // `Once` does not grant the class for later calls.
    assert!(h.agent.policy().requires_approval(ApprovalClass::Write));
}

#[tokio::test]
async fn agent_denied_call_returns_the_denial_text_and_changes_nothing() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[("write_file", json!({"path": "out.txt", "content": "x"}))],
        ),
        Scripted::text("ok, I will not write"),
    ]));
    let (reason, seen) = run_turn(&mut h.agent, "write it", CancellationToken::new(), |_| {
        Decision::Deny
    })
    .await;
    assert_eq!(reason, StopReason::Completed);
    assert!(!h.root.join("out.txt").exists());
    assert_eq!(
        tool_results(h.agent.history()),
        vec![("write_file".to_owned(), DENIED_TEXT.to_owned(), true)]
    );
    assert!(seen.iter().any(|s| matches!(s,
        Seen::ToolFinished { output, is_error: true } if output == DENIED_TEXT)));
    // The model saw the denial on its next request.
    let second = &h.mock.requests()[1];
    assert!(
        matches!(second.history.last(), Some(Item::ToolResult { output, .. }) if output == DENIED_TEXT)
    );
}

#[tokio::test]
async fn agent_session_decision_stops_later_prompts() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[
                ("write_file", json!({"path": "a.txt", "content": "a"})),
                ("write_file", json!({"path": "b.txt", "content": "b"})),
            ],
        ),
        Scripted::tool_calls(
            "",
            &[("write_file", json!({"path": "c.txt", "content": "c"}))],
        ),
        Scripted::text("done"),
    ]));
    let mut asked = 0;
    let (reason, _) = run_turn(
        &mut h.agent,
        "write three",
        CancellationToken::new(),
        |_| {
            asked += 1;
            Decision::Session
        },
    )
    .await;
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(asked, 1, "only the first write asks");
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(h.root.join(name).exists(), "{name}");
    }
}

#[tokio::test]
async fn agent_dropped_approval_sender_counts_as_denial() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("run_shell", json!({"command": "touch ran"}))]),
        Scripted::text("ok"),
    ]));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let run = h.agent.run_turn("go", tx, CancellationToken::new());
    let drop_requests = async {
        while let Some(event) = rx.recv().await {
            drop(event);
        }
    };
    let (reason, ()) = tokio::join!(run, drop_requests);
    assert_eq!(reason, StopReason::Completed);
    assert!(!h.root.join("ran").exists());
    assert_eq!(tool_results(h.agent.history())[0].1, DENIED_TEXT);
}

#[tokio::test]
async fn agent_read_tools_and_policy_overrides() {
    // Shell allowed by policy runs without a prompt.
    let policy = ApprovalPolicy::default().with_default(ApprovalClass::Shell, PolicyAction::Allow);
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("run_shell", json!({"command": "echo hi"}))]),
        Scripted::text("done"),
    ]);
    opts.policy = policy;
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "go", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    assert!(tool_results(h.agent.history())[0].1.contains("hi"));
}

#[tokio::test]
async fn agent_unknown_tool_and_bad_arguments_are_fed_back_as_errors() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[
                ("no_such_tool", json!({})),
                ("read_file", json!("{not json")),
                ("read_file", json!({"path": "../escape"})),
            ],
        ),
        Scripted::text("sorry"),
    ]));
    let (reason, _) = run_turn(&mut h.agent, "go", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    let results = tool_results(h.agent.history());
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.2), "{results:?}");
    assert!(results[0].1.contains("Unknown tool"));
    assert!(results[1].1.contains("not valid JSON"));
    assert!(results[2].1.contains("outside the workspace"));
}

#[tokio::test]
async fn agent_tool_results_are_truncated_when_configured() {
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("read_file", json!({"path": "big.txt"}))]),
        Scripted::text("ok"),
    ]);
    opts.tool_result_max_chars = Some(30);
    let mut h = harness(opts);
    std::fs::write(h.root.join("big.txt"), "x".repeat(500)).expect("write");
    let (reason, seen) = run_turn(&mut h.agent, "go", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    let output = &tool_results(h.agent.history())[0].1;
    assert!(output.chars().count() < 80, "{output}");
    assert!(output.contains("[truncated "), "{output}");
    assert!(
        seen.iter()
            .any(|s| matches!(s, Seen::ToolFinished { output: o, .. } if o == output))
    );
}

#[tokio::test]
async fn agent_max_steps_reached_stops_with_a_valid_history() {
    let call = || Scripted::tool_calls("", &[("list_dir", json!({}))]);
    let mut opts = HarnessOpts::new(vec![
        call(),
        call(),
        call(),
        Scripted::text("never reached"),
    ]);
    opts.max_steps = 3;
    let mut h = harness(opts);
    let (reason, seen) = run_turn(&mut h.agent, "loop", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::MaxSteps);
    assert_eq!(h.mock.call_count(), 3);
    assert_eq!(seen.last(), Some(&Seen::Finished(StopReason::MaxSteps)));
    // Every tool call has its result, so the conversation can continue.
    assert!(matches!(
        h.agent.history().last(),
        Some(Item::ToolResult { .. })
    ));
}

fn benchmark(max_no_tool_turns: u32) -> StopPolicy {
    StopPolicy::Benchmark {
        finish_marker: "[FINISH]".to_owned(),
        nudge_text: "Continue or say [FINISH].".to_owned(),
        max_no_tool_turns,
    }
}

fn nudges(history: &[Item]) -> usize {
    history
        .iter()
        .filter(|i| matches!(i, Item::User { text } if text == "Continue or say [FINISH]."))
        .count()
}

#[tokio::test]
async fn agent_benchmark_policy_stops_on_the_finish_marker() {
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("working", &[("list_dir", json!({}))]),
        Scripted::text("all done [FINISH]"),
        Scripted::text("must not be requested"),
    ]);
    opts.stop_policy = benchmark(3);
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "task", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(h.mock.call_count(), 2);
    assert_eq!(nudges(h.agent.history()), 0);

    // A marker alongside tool calls still runs the calls, then stops.
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("done [FINISH]", &[("list_dir", json!({}))]),
        Scripted::text("must not be requested"),
    ]);
    opts.stop_policy = benchmark(3);
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "task", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(h.mock.call_count(), 1);
    assert!(matches!(
        h.agent.history().last(),
        Some(Item::ToolResult { .. })
    ));
}

#[tokio::test]
async fn agent_benchmark_nudges_twice_then_stops_on_the_third_no_tool_turn() {
    let mut opts = HarnessOpts::new(vec![
        Scripted::text("hmm"),
        Scripted::text("still thinking"),
        Scripted::text("third turn without tools"),
        Scripted::text("must not be requested"),
    ]);
    opts.stop_policy = benchmark(3);
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "task", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Stalled);
    assert_eq!(h.mock.call_count(), 3);
    assert_eq!(
        nudges(h.agent.history()),
        2,
        "nudged after the first two turns only"
    );
    // The model saw each nudge on the following request.
    let requests = h.mock.requests();
    assert_eq!(nudges(&requests[1].history), 1);
    assert_eq!(nudges(&requests[2].history), 2);

    // A turn with neither text nor tool calls stops at once.
    let mut opts = HarnessOpts::new(vec![Scripted::text(""), Scripted::text("never")]);
    opts.stop_policy = benchmark(3);
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "task", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Stalled);
    assert_eq!(h.mock.call_count(), 1);
}

#[tokio::test]
async fn agent_budget_cap_refuses_the_call_before_it_is_sent() {
    // Reservation for sol: ~1000 output tokens at $10/Mtok plus input = about $0.01.
    let mut opts = HarnessOpts::new(vec![Scripted::text("never")]);
    opts.cap_usd = 0.001;
    let mut h = harness(opts);
    let (reason, seen) = run_turn(&mut h.agent, "hi", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Budget);
    assert_eq!(h.mock.call_count(), 0, "no model call is made");
    assert!(
        seen.iter()
            .any(|s| matches!(s, Seen::Notice(n) if n.contains("dispatch cap")))
    );
    assert_eq!(seen.last(), Some(&Seen::Finished(StopReason::Budget)));
    assert!(h.ledger.lock().unwrap().records().is_empty());
}

#[tokio::test]
async fn agent_budget_cap_stops_a_turn_after_the_spend_uses_it_up() {
    // First call fits (reserve ~$0.0101); it actually costs 2000 * $10/Mtok = $0.02, leaving
    // $0.005 against the $0.025 cap, so the second reservation is refused.
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls("", &[("list_dir", json!({}))]).with_usage(usage(10, 2000)),
        Scripted::text("never"),
    ]);
    opts.cap_usd = 0.025;
    let mut h = harness(opts);
    let (reason, _) = run_turn(&mut h.agent, "hi", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Budget);
    assert_eq!(h.mock.call_count(), 1);
    assert!(matches!(
        h.agent.history().last(),
        Some(Item::ToolResult { .. })
    ));
}

#[tokio::test]
async fn agent_cancel_during_a_model_call_appends_no_assistant_turn() {
    let mut h = harness(HarnessOpts::new(vec![Scripted::Pending]));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        trigger.cancel();
    });
    let (reason, seen) = run_turn(&mut h.agent, "hi", cancel, no_prompt).await;
    assert_eq!(reason, StopReason::Cancelled);
    assert_eq!(seen.last(), Some(&Seen::Finished(StopReason::Cancelled)));
    assert_eq!(h.agent.history().len(), 1, "only the user message");
    assert!(matches!(&h.agent.history()[0], Item::User { .. }));
    // The call may already have been billed, so the reservation is NOT released: it is settled
    // as unknown usage and keeps counting against the cap.
    let ledger = h.ledger.lock().unwrap();
    assert!(ledger.outstanding_reserved_usd().abs() < 1e-12);
    assert_eq!(ledger.records().len(), 1);
    assert_eq!(ledger.records()[0].cost_status, CostStatus::Unknown);
    assert!(ledger.records()[0].estimated_usd.is_none());
    assert!(ledger.unreconciled_usd() > 0.0);
    assert!(
        ledger.spent_usd().abs() < 1e-12,
        "unknown cost is not reported as spend"
    );
}

#[tokio::test]
async fn agent_cancel_during_a_tool_keeps_the_history_valid() {
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[
                ("run_shell", json!({"command": "sleep 20"})),
                ("list_dir", json!({})),
            ],
        ),
        Scripted::text("never"),
    ]);
    opts.policy = ApprovalPolicy::allow_all();
    let mut h = harness(opts);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let (reason, _) = run_turn(&mut h.agent, "go", cancel, no_prompt).await;
    assert_eq!(reason, StopReason::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let results = tool_results(h.agent.history());
    assert_eq!(results.len(), 2, "both calls get a result: {results:?}");
    assert!(results[0].1.contains("cancelled"), "{results:?}");
    assert!(results[1].1.contains("Cancelled"), "{results:?}");
    assert_eq!(h.mock.call_count(), 1);
}

#[tokio::test]
async fn agent_model_errors_end_the_turn_and_settle_the_ledger() {
    // A plain HTTP error: the reservation is released.
    let mut h = harness(HarnessOpts::new(vec![Scripted::error(ModelError::Http {
        status: 401,
        message: "bad key".into(),
    })]));
    let (reason, seen) = run_turn(&mut h.agent, "hi", CancellationToken::new(), no_prompt).await;
    assert!(
        matches!(&reason, StopReason::Error(m) if m.contains("401")),
        "{reason:?}"
    );
    assert!(
        seen.iter()
            .any(|s| matches!(s, Seen::Error(m) if m.contains("bad key")))
    );
    {
        let ledger = h.ledger.lock().unwrap();
        assert!(ledger.outstanding_reserved_usd().abs() < 1e-12);
        assert!(ledger.unreconciled_usd().abs() < 1e-12);
    }

    // A timeout may have been billed: the reservation stays counted as unreconciled.
    let mut h = harness(HarnessOpts::new(vec![Scripted::error(ModelError::Timeout)]));
    let (reason, _) = run_turn(&mut h.agent, "hi", CancellationToken::new(), no_prompt).await;
    assert!(matches!(reason, StopReason::Error(_)));
    let ledger = h.ledger.lock().unwrap();
    assert!(ledger.unreconciled_usd() > 0.0);
    assert_eq!(ledger.records().len(), 1);
}

#[tokio::test]
async fn agent_set_model_validates_and_keeps_history() {
    let mut h = harness(HarnessOpts::new(vec![
        Scripted::text("first"),
        Scripted::text("second"),
    ]));
    let (reason, _) = run_turn(&mut h.agent, "one", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);

    assert!(matches!(
        h.agent.set_model("argon", Effort::High),
        Err(AgentError::Registry(_))
    ));
    assert!(matches!(
        h.agent.set_model("sol", Effort::None),
        Err(AgentError::UnsupportedEffort { .. })
    ));
    assert!(matches!(
        h.agent.set_model("flash", Effort::Minimal),
        Err(AgentError::UnsupportedEffort { .. })
    ));
    assert_eq!(
        h.agent.config().model_alias,
        "sol",
        "failed switches change nothing"
    );

    h.agent
        .set_model("flash", Effort::Low)
        .expect("valid switch");
    let (reason, _) = run_turn(&mut h.agent, "two", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    let requests = h.mock.requests();
    assert_eq!(requests[1].model_alias, "flash");
    assert_eq!(requests[1].effort, Effort::Low);
    assert_eq!(requests[1].history.len(), 3, "history survives the switch");
    // The first turn keeps the alias of the model that produced it.
    assert!(matches!(&h.agent.history()[1], Item::Assistant(t) if t.model_alias == "sol"));
    assert!(matches!(&h.agent.history()[3], Item::Assistant(t) if t.model_alias == "flash"));
    // Both calls were priced from their own rate entries.
    assert_eq!(h.ledger.lock().unwrap().records().len(), 2);
}

#[tokio::test]
async fn agent_construction_errors() {
    use harness_agent::agent::{Agent, AgentConfig, AgentParts};
    use harness_agent::tools::{NamingMode, ToolRegistry};
    use std::collections::HashMap;
    let parts = || AgentParts {
        models: common::registry(),
        clients: HashMap::new(),
        tools: ToolRegistry::new(NamingMode::Bare),
        builtins: None,
        ledger: Arc::new(std::sync::Mutex::new(harness_core::cost::CostLedger::new(
            1.0,
            harness_core::cost::RateCard::empty("r", "2026-10-04"),
        ))),
        policy: ApprovalPolicy::default(),
        transcript: None,
    };
    let bad_effort = Agent::new(AgentConfig::new("sol", Effort::None, "x"), parts());
    assert!(matches!(
        bad_effort,
        Err(AgentError::UnsupportedEffort { .. })
    ));
    let bad_alias = Agent::new(AgentConfig::new("zzz", Effort::High, "x"), parts());
    assert!(matches!(bad_alias, Err(AgentError::Registry(_))));
    let no_workspace = Agent::new(AgentConfig::new("sol", Effort::High, "x"), parts());
    assert!(matches!(no_workspace, Err(AgentError::Tools(_))));
    let mut config = AgentConfig::new("sol", Effort::High, "x");
    config.builtins_enabled = false;
    let agent = Agent::new(config, parts()).expect("no built-ins needed");
    assert!(agent.tools().is_empty());
}

#[tokio::test]
async fn agent_without_a_client_for_the_provider_reports_an_error() {
    use harness_agent::agent::{Agent, AgentConfig, AgentParts};
    use harness_agent::tools::{NamingMode, ToolRegistry};
    use std::collections::HashMap;
    let mut config = AgentConfig::new("sol", Effort::High, "x");
    config.builtins_enabled = false;
    let models = common::registry();
    let ledger = Arc::new(std::sync::Mutex::new(harness_core::cost::CostLedger::new(
        10.0,
        harness_agent::pricing::rate_card(&models),
    )));
    let mut agent = Agent::new(
        config,
        AgentParts {
            models,
            clients: HashMap::new(),
            tools: ToolRegistry::new(NamingMode::Bare),
            builtins: None,
            ledger,
            policy: ApprovalPolicy::default(),
            transcript: None,
        },
    )
    .expect("agent");
    let (reason, seen) = run_turn(&mut agent, "hi", CancellationToken::new(), no_prompt).await;
    assert!(
        matches!(&reason, StopReason::Error(m) if m.contains("no client")),
        "{reason:?}"
    );
    assert!(seen.iter().any(|s| matches!(s, Seen::Error(_))));
}

#[tokio::test]
async fn agent_adds_the_registry_api_key_variables_to_the_shell_scrub_list() {
    // The test harness builds BuiltinTools with an EMPTY scrub list.
    let mut opts = HarnessOpts::new(vec![
        Scripted::tool_calls(
            "",
            &[(
                "run_shell",
                json!({"command": "[ -z \"${OPENAI_API_KEY+x}${GEMINI_API_KEY+x}\" ] && echo absent || echo present"}),
            )],
        ),
        Scripted::text("done"),
    ]);
    opts.policy = ApprovalPolicy::allow_all();
    let mut h = harness(opts);

    let scrub = h.agent.builtins().expect("builtins configured").scrub_env();
    assert!(scrub.contains("OPENAI_API_KEY"), "{scrub:?}");
    assert!(scrub.contains("GEMINI_API_KEY"), "{scrub:?}");

    let (reason, _) = run_turn(&mut h.agent, "env", CancellationToken::new(), no_prompt).await;
    assert_eq!(reason, StopReason::Completed);
    let output = &tool_results(h.agent.history())[0].1;
    assert!(output.contains("absent"), "{output}");
    assert!(!output.contains("present"), "{output}");
}
