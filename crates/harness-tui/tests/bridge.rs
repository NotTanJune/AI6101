//! The agent bridge: a real `AgentHandle` over the mock model client, with an in-process MCP
//! server. No network, no real provider.

mod common;

use std::time::Duration;

use common::*;
use harness_agent::approval::{ApprovalClass, ApprovalRequest, DENIED_TEXT, Decision};
use harness_agent::events::{AgentEvent, StopReason};
use harness_agent::model::mock::Scripted;
use harness_agent::registry::Effort;
use harness_core::{CanaryStatus, Strategy};
use harness_tui::{
    AgentPort, AgentUiEvent, App, CanarySummary, McpServerStatus, McpState, Msg,
    ScriptedAgentHandle, UiCommand, UiEvent, split_agent_event,
};
use serde_json::json;
use tokio::sync::oneshot;

fn write_call(path: &str, content: &str) -> Scripted {
    Scripted::tool_calls(
        "writing",
        &[("write_file", json!({"path": path, "content": content}))],
    )
}

fn finished_with(events: &[UiEvent]) -> Option<&StopReason> {
    events.iter().rev().find_map(|e| match e {
        UiEvent::Agent(AgentUiEvent::TurnFinished { reason }) => Some(reason),
        _ => None,
    })
}

fn approvals(events: &[UiEvent]) -> Vec<(u64, ApprovalRequest)> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::ApprovalNeeded { id, request } => Some((*id, request.clone())),
            _ => None,
        })
        .collect()
}

fn last_status<'a>(events: &'a [UiEvent], name: &str) -> Option<&'a McpServerStatus> {
    events.iter().rev().find_map(|e| match e {
        UiEvent::McpStatus(s) if s.name == name => Some(s),
        _ => None,
    })
}

fn exposed_description(status: &McpServerStatus, tool: &str) -> Option<String> {
    status
        .exposed
        .iter()
        .find(|t| t.name == tool)
        .map(|t| t.description.clone())
}

fn notices(events: &[UiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::Notice(t) | UiEvent::Agent(AgentUiEvent::Notice(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn errors(events: &[UiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::Error(t) | UiEvent::Agent(AgentUiEvent::Error(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn feed(app: &mut App, events: &[UiEvent]) {
    for event in events {
        app.update(Msg::Ui(event.clone()));
    }
}

fn transcript_lines(f: &Fixture) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(f.session_dir.join("transcript.jsonl")).expect("transcript");
    text.lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

// -------------------------------------------------------------------------------------------

#[test]
fn a_text_reply_streams_through_the_bridge_into_the_app() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![Scripted::text(
        "Hi there",
    )])));
    let mut app = f.app();
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("hello".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );

    // The runtime announces itself, then streams the turn.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, UiEvent::ModelChanged { alias, effort }
        if alias == "luna" && *effort == Effort::Max))
    );
    assert!(events.iter().any(|e| matches!(e, UiEvent::KbSummary(_))));
    assert_eq!(finished_with(&events), Some(&StopReason::Completed));
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            UiEvent::Agent(AgentUiEvent::TurnStarted) => Some("started"),
            UiEvent::Agent(AgentUiEvent::TextDelta(_)) => Some("text"),
            UiEvent::Agent(AgentUiEvent::Usage { .. }) => Some("usage"),
            UiEvent::Agent(AgentUiEvent::TurnFinished { .. }) => Some("finished"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec!["started", "text", "usage", "finished"]);

    feed(&mut app, &events);
    assert!(app.transcript.iter().any(|e| matches!(e,
        harness_tui::Entry::Assistant { text, streaming: false } if text == "Hi there")));
    assert!(!app.busy);
    assert_eq!((app.status.tokens_in, app.status.tokens_out), (100, 20));
    // Cost: (100 * 0.1 + 20 * 0.5) / 1e6 for luna.
    assert!(
        (app.status.spent_usd - 0.00002).abs() < 1e-9,
        "{}",
        app.status.spent_usd
    );

    assert!(
        f.handle.shutdown(Duration::from_secs(5)),
        "the runtime stops cleanly"
    );
    assert!(
        f.handle.shutdown(Duration::from_secs(5)),
        "shutdown is idempotent"
    );
}

#[test]
fn an_approval_round_trip_runs_the_tool_only_after_the_user_answers() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![
        write_call("hello.txt", "hello\n"),
        Scripted::text("done"),
    ])));
    let mut events = Vec::new();
    f.handle
        .send(UiCommand::SendUser("create hello.txt".to_owned()));
    wait_until(&mut f.handle, &mut events, "an approval request", |e| {
        !approvals(e).is_empty()
    });

    let (id, request) = approvals(&events).remove(0);
    assert_eq!(request.tool, "write_file");
    assert_eq!(request.class, ApprovalClass::Write);
    assert!(request.summary.contains("hello.txt"), "{}", request.summary);
    let diff = request.diff.expect("a write carries a diff");
    assert!(diff.contains("+hello"), "{diff}");
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !f.workspace.join("hello.txt").exists(),
        "nothing runs before the answer"
    );
    assert!(
        finished_with(&events).is_none(),
        "the turn waits for the user"
    );

    f.handle.send(UiCommand::Approve {
        id,
        decision: Decision::Once,
    });
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Completed));
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("hello.txt")).expect("file"),
        "hello\n"
    );
    let call_result = events.iter().find_map(|e| match e {
        UiEvent::Agent(AgentUiEvent::ToolCallFinished { is_error, .. }) => Some(*is_error),
        _ => None,
    });
    assert_eq!(call_result, Some(false));
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn denying_an_approval_refuses_the_call_and_the_model_hears_about_it() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![
        write_call("nope.txt", "x"),
        Scripted::text("understood"),
    ])));
    let mut app = f.app();
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("write it".to_owned()));
    wait_until(&mut f.handle, &mut events, "an approval request", |e| {
        !approvals(e).is_empty()
    });
    let (id, _) = approvals(&events).remove(0);
    f.handle.send(UiCommand::Approve {
        id,
        decision: Decision::Deny,
    });
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );

    assert!(!f.workspace.join("nope.txt").exists());
    let output = events.iter().find_map(|e| match e {
        UiEvent::Agent(AgentUiEvent::ToolCallFinished {
            output,
            is_error: true,
            ..
        }) => Some(output.clone()),
        _ => None,
    });
    assert_eq!(output.as_deref(), Some(DENIED_TEXT));
    // The follow-up model call saw the denial as a tool result.
    let requests = f.mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(format!("{:?}", requests[1].history).contains(DENIED_TEXT));

    feed(&mut app, &events);
    let card = app.transcript.iter().find_map(|e| match e {
        harness_tui::Entry::ToolCard(c) => Some(c.clone()),
        _ => None,
    });
    let card = card.expect("a tool card");
    assert!(card.is_error && card.output.as_deref() == Some(DENIED_TEXT));
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn a_session_approval_covers_later_calls_of_the_same_class() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![
        write_call("a.txt", "a"),
        write_call("b.txt", "b"),
        Scripted::text("both written"),
    ])));
    let mut events = Vec::new();
    f.handle
        .send(UiCommand::SendUser("write two files".to_owned()));
    wait_until(&mut f.handle, &mut events, "an approval request", |e| {
        !approvals(e).is_empty()
    });
    let (id, _) = approvals(&events).remove(0);
    f.handle.send(UiCommand::Approve {
        id,
        decision: Decision::Session,
    });
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(
        approvals(&events).len(),
        1,
        "the second write was not asked about"
    );
    assert!(f.workspace.join("a.txt").exists() && f.workspace.join("b.txt").exists());
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn cancelling_mid_turn_ends_it_and_the_ledger_shows_unreconciled_spend() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![
        Scripted::Pending,
        Scripted::text("back again"),
    ])));
    let mut app = f.app();
    let mut events = Vec::new();
    f.handle
        .send(UiCommand::SendUser("think forever".to_owned()));
    wait_until(&mut f.handle, &mut events, "the turn to start", |e| {
        e.iter()
            .any(|e| matches!(e, UiEvent::Agent(AgentUiEvent::TurnStarted)))
    });
    f.handle.send(UiCommand::Cancel);
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Cancelled));

    let cost = events.iter().rev().find_map(|e| match e {
        UiEvent::Cost(c) => Some(*c),
        _ => None,
    });
    let cost = cost.expect("a cost update follows the turn");
    assert!(
        cost.unreconciled_usd > 0.0,
        "a cancelled call counts at its worst case: {cost:?}"
    );
    assert!(f.ledger.lock().expect("ledger").unreconciled_usd() > 0.0);
    feed(&mut app, &events);
    assert!(!app.busy);
    assert!(
        app.transcript
            .iter()
            .any(|e| matches!(e, harness_tui::Entry::Notice(t) if t.contains("cancelled")))
    );
    assert!(app.cost.unreconciled_usd > 0.0);

    // The runtime is still usable.
    events.clear();
    f.handle.send(UiCommand::SendUser("again".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the next turn",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Completed));
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn cancelling_while_an_approval_is_pending_denies_it_and_runs_nothing() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![write_call(
        "late.txt", "x",
    )])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("write".to_owned()));
    wait_until(&mut f.handle, &mut events, "an approval request", |e| {
        !approvals(e).is_empty()
    });
    f.handle.send(UiCommand::Cancel);
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Cancelled));
    assert!(!f.workspace.join("late.txt").exists());
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn a_stale_approval_answer_is_harmless() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![Scripted::text("ok")])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::Approve {
        id: 99,
        decision: Decision::Once,
    });
    f.handle.send(UiCommand::Cancel);
    f.handle.send(UiCommand::SendUser("hi".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Completed));
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn model_and_effort_changes_are_validated_and_used_by_the_next_call() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![Scripted::text(
        "from sol",
    )])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::SetModel {
        alias: "sol".to_owned(),
        effort: Effort::High,
    });
    f.handle.send(UiCommand::SetModel {
        alias: "sol".to_owned(),
        effort: Effort::None,
    });
    f.handle.send(UiCommand::SetModel {
        alias: "ghost".to_owned(),
        effort: Effort::High,
    });
    wait_until(&mut f.handle, &mut events, "three answers", |e| {
        e.iter().filter(|e| matches!(e, UiEvent::Error(_))).count() == 2
            && e.iter()
                .any(|e| matches!(e, UiEvent::ModelChanged { alias, .. } if alias == "sol"))
    });
    let errors = errors(&events);
    assert!(
        errors.iter().any(|e| e.contains("does not support effort")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("unknown model alias")),
        "{errors:?}"
    );

    f.handle.send(UiCommand::SendUser("hi".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    let request = f.mock.requests().remove(0);
    assert_eq!(request.model_alias, "sol");
    assert_eq!(
        request.effort,
        Effort::High,
        "the rejected change did not stick"
    );
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn commands_that_change_the_agent_are_ignored_while_a_turn_runs() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![Scripted::Pending])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("long".to_owned()));
    wait_until(&mut f.handle, &mut events, "the turn to start", |e| {
        e.iter()
            .any(|e| matches!(e, UiEvent::Agent(AgentUiEvent::TurnStarted)))
    });
    f.handle.send(UiCommand::SetModel {
        alias: "sol".to_owned(),
        effort: Effort::High,
    });
    f.handle
        .send(UiCommand::SendUser("second message".to_owned()));
    wait_until(&mut f.handle, &mut events, "two notices", |e| {
        notices(e)
            .iter()
            .filter(|n| n.contains("ignored while a turn is running"))
            .count()
            == 2
    });
    f.handle.send(UiCommand::Cancel);
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, UiEvent::ModelChanged { alias, .. } if alias == "sol")),
        "the model change was ignored, not queued"
    );
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn shutdown_during_a_running_turn_cancels_it_and_stops_promptly() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![Scripted::Pending])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("long".to_owned()));
    wait_until(&mut f.handle, &mut events, "the turn to start", |e| {
        e.iter()
            .any(|e| matches!(e, UiEvent::Agent(AgentUiEvent::TurnStarted)))
    });
    let started = std::time::Instant::now();
    assert!(f.handle.shutdown(Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn a_tiny_budget_ends_the_turn_with_a_notice_instead_of_a_call() {
    let mut opts = SetupOpts::new(Script::Replies(vec![Scripted::text("never sent")]));
    opts.cap_usd = 1e-9;
    let mut f = spawn(opts);
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("hi".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Budget));
    assert_eq!(f.mock.call_count(), 0, "the model was never called");
    assert!(!notices(&events).is_empty(), "the user is told why");
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn the_session_transcript_records_the_conversation_with_the_schema_tag() {
    let mut f = spawn(SetupOpts::new(Script::Replies(vec![
        write_call("t.txt", "t"),
        Scripted::text("finished"),
    ])));
    let mut events = Vec::new();
    f.handle.send(UiCommand::SendUser("write t".to_owned()));
    wait_until(&mut f.handle, &mut events, "an approval request", |e| {
        !approvals(e).is_empty()
    });
    let (id, _) = approvals(&events).remove(0);
    f.handle.send(UiCommand::Approve {
        id,
        decision: Decision::Deny,
    });
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    f.handle.shutdown(Duration::from_secs(5));

    let lines = transcript_lines(&f);
    assert!(!lines.is_empty());
    assert!(
        lines.iter().all(|l| l["schema"] == "agent-session/1"),
        "{lines:?}"
    );
    let kinds: Vec<&str> = lines.iter().filter_map(|l| l["kind"].as_str()).collect();
    for kind in [
        "session_start",
        "user_message",
        "assistant_message",
        "tool_call",
        "approval",
        "tool_result",
        "turn_finished",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    let approval = lines
        .iter()
        .find(|l| l["kind"] == "approval")
        .expect("approval line");
    assert_eq!(approval["decision"], "deny");
}

// -------------------------------------------------------------------------------------------
// MCP
// -------------------------------------------------------------------------------------------

#[test]
fn managed_servers_expose_kb_tools_and_unmanaged_servers_expose_none() {
    let mut opts = SetupOpts::new(Script::Replies(vec![
        Scripted::tool_calls("", &[("mcp__calc__add", json!({"a": 2, "b": 40}))]),
        Scripted::text("42"),
    ]));
    opts.servers = vec!["calc", "docs"];
    opts.kb = Some(kb(vec![card("calc", "add", "KB add description")]));
    let mut f = spawn(opts);
    let mut events = Vec::new();
    wait_until(&mut f.handle, &mut events, "both servers connected", |e| {
        ["calc", "docs"]
            .iter()
            .all(|n| last_status(e, n).is_some_and(|s| s.state == McpState::Connected))
    });
    // `Connected` is sent twice per server (after tools are registered); read the final state.
    let calc = last_status(&events, "calc").expect("calc status").clone();
    assert!(calc.managed);
    assert_eq!(calc.live_tools, 4);
    assert_eq!(
        exposed_description(&calc, "mcp__calc__add").as_deref(),
        Some("KB add description"),
        "the model sees the KB description, not the live one"
    );
    assert_eq!(calc.exposed.len(), 1, "only KB cards are exposed");
    let docs = last_status(&events, "docs").expect("docs status");
    assert!(!docs.managed);
    assert_eq!(docs.live_tools, 4);
    assert!(
        docs.exposed.is_empty(),
        "an unmanaged server exposes no tools"
    );
    // The KB differs from the live server: drift is reported for the managed server only.
    let drifted: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            UiEvent::DriftReady { server, report } if !report.is_empty() => Some(server.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(drifted, vec!["calc"]);

    // A managed tool can be called through the namespaced name.
    f.handle
        .send(UiCommand::SendUser("add 2 and 40".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    let output = events.iter().find_map(|e| match e {
        UiEvent::Agent(AgentUiEvent::ToolCallFinished {
            output, is_error, ..
        }) => Some((output.clone(), *is_error)),
        _ => None,
    });
    assert_eq!(output, Some(("42".to_owned(), false)));
    assert!(
        f.mock.requests()[0]
            .tool_names
            .contains(&"mcp__calc__add".to_owned())
    );
    assert!(
        !f.mock.requests()[0]
            .tool_names
            .iter()
            .any(|n| n.starts_with("mcp__docs__"))
    );
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn a_server_that_cannot_start_is_reported_and_the_session_continues() {
    let mut opts = SetupOpts::new(Script::Replies(vec![Scripted::text("still works")]));
    opts.servers = vec!["bad", "docs"];
    opts.failing_servers = vec!["bad".to_owned()];
    let mut f = spawn(opts);
    let mut events = Vec::new();
    wait_until(
        &mut f.handle,
        &mut events,
        "docs connected and bad failed",
        |e| {
            last_status(e, "docs").is_some_and(|s| s.state == McpState::Connected)
                && last_status(e, "bad").is_some_and(|s| matches!(s.state, McpState::Failed(_)))
        },
    );
    let errors = errors(&events);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("MCP server bad") && e.contains("no such program")),
        "{errors:?}"
    );
    f.handle.send(UiCommand::ConnectMcp("nope".to_owned()));
    wait_until(&mut f.handle, &mut events, "an unknown-server error", |e| {
        errors_contain(e, "no MCP server named \"nope\"")
    });
    f.handle.send(UiCommand::SendUser("hi".to_owned()));
    wait_until(
        &mut f.handle,
        &mut events,
        "the turn to finish",
        has_turn_finished,
    );
    assert_eq!(finished_with(&events), Some(&StopReason::Completed));
    f.handle.shutdown(Duration::from_secs(5));
}

fn errors_contain(events: &[UiEvent], needle: &str) -> bool {
    errors(events).iter().any(|e| e.contains(needle))
}

#[test]
fn disconnecting_removes_the_tools_and_connecting_restores_them() {
    let mut opts = SetupOpts::new(Script::Replies(vec![]));
    opts.servers = vec!["calc"];
    opts.kb = Some(kb(vec![card("calc", "add", "KB add description")]));
    let mut f = spawn(opts);
    let mut events = Vec::new();
    wait_until(&mut f.handle, &mut events, "calc connected", |e| {
        last_status(e, "calc")
            .is_some_and(|s| s.state == McpState::Connected && !s.exposed.is_empty())
    });
    events.clear();
    f.handle.send(UiCommand::DisconnectMcp("calc".to_owned()));
    wait_until(&mut f.handle, &mut events, "calc disconnected", |e| {
        last_status(e, "calc").is_some_and(|s| s.state == McpState::Disconnected)
    });
    let status = last_status(&events, "calc").expect("status");
    assert!(status.exposed.is_empty() && status.live_tools == 0);
    events.clear();
    f.handle.send(UiCommand::ConnectMcp("calc".to_owned()));
    wait_until(&mut f.handle, &mut events, "calc reconnected", |e| {
        last_status(e, "calc")
            .is_some_and(|s| s.state == McpState::Connected && !s.exposed.is_empty())
    });
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn adopting_an_unmanaged_server_snapshots_its_live_tools_into_the_kb() {
    let mut opts = SetupOpts::new(Script::Replies(vec![]));
    opts.servers = vec!["docs"];
    let mut f = spawn(opts);
    let mut events = Vec::new();
    wait_until(&mut f.handle, &mut events, "docs connected", |e| {
        last_status(e, "docs").is_some_and(|s| s.state == McpState::Connected)
    });
    assert!(
        last_status(&events, "docs")
            .expect("status")
            .exposed
            .is_empty()
    );
    events.clear();
    f.handle.send(UiCommand::AdoptServer("docs".to_owned()));
    wait_until(&mut f.handle, &mut events, "docs adopted", |e| {
        last_status(e, "docs").is_some_and(|s| s.managed)
    });
    let docs = last_status(&events, "docs").expect("status");
    assert_eq!(docs.exposed.len(), 4);
    assert_eq!(
        exposed_description(docs, "mcp__docs__add").as_deref(),
        Some("LIVE add description"),
        "the snapshot is of the live definitions"
    );
    let summary = events.iter().rev().find_map(|e| match e {
        UiEvent::KbSummary(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        summary.expect("kb summary").servers,
        vec![("docs".to_owned(), 4)]
    );

    // Adopting twice is refused.
    events.clear();
    f.handle.send(UiCommand::AdoptServer("docs".to_owned()));
    wait_until(&mut f.handle, &mut events, "a refusal", |e| {
        errors_contain(e, "already managed")
    });
    f.handle.shutdown(Duration::from_secs(5));
}

// -------------------------------------------------------------------------------------------
// Drift
// -------------------------------------------------------------------------------------------

/// A KB that disagrees with the in-process server, and the rewrite a model would return.
fn drift_setup(script: impl FnOnce(String) -> Vec<Scripted>) -> (Fixture, String) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let source = kb(vec![card("calc", "add", "KB add description")]);
    let rewritten = runtime.block_on(async {
        let conn = connect_test_server("calc").await;
        let live = conn.list_tools().await.expect("list");
        let (_, live_kb) = harness_agent::drift::detect_drift("calc", &source, &live);
        serde_json::to_string(&live_kb).expect("serialise")
    });
    let mut opts = SetupOpts::new(Script::Replies(script(rewritten.clone())));
    opts.servers = vec!["calc"];
    opts.kb = Some(source);
    (spawn(opts), rewritten)
}

fn wait_for_drift(f: &mut Fixture, events: &mut Vec<UiEvent>) {
    wait_until(&mut f.handle, events, "drift on calc", |e| {
        e.iter()
            .any(|e| matches!(e, UiEvent::DriftReady { report, .. } if !report.is_empty()))
    });
}

fn canary_of(events: &[UiEvent]) -> Option<&CanarySummary> {
    events.iter().rev().find_map(|e| match e {
        UiEvent::CanaryReady(c) => Some(c),
        _ => None,
    })
}

#[test]
fn drift_end_to_end_diff_strategy_canary_accept_changes_the_exposed_description() {
    let (mut f, _) = drift_setup(|rewritten| vec![Scripted::text(rewritten)]);
    let mut app = f.app();
    let mut events = Vec::new();
    wait_for_drift(&mut f, &mut events);
    wait_until(&mut f.handle, &mut events, "calc connected", |e| {
        last_status(e, "calc").is_some_and(|s| s.state == McpState::Connected)
    });
    feed(&mut app, &events);

    // 1. The diff: description and schema changed, three tools are new on the server.
    let report = events.iter().find_map(|e| match e {
        UiEvent::DriftReady { report, .. } if !report.is_empty() => Some(report.clone()),
        _ => None,
    });
    let report = report.expect("a drift report");
    assert_eq!(report.server, "calc");
    assert!(
        report
            .diff
            .changes
            .iter()
            .any(|c| c.tool_id == "add" && c.field_path.as_deref() == Some("description"))
    );
    assert!(report.diff.changes.len() >= 2);
    assert_eq!(app.status.drift_count, 1);
    let before = last_status(&events, "calc").expect("status");
    assert_eq!(
        exposed_description(before, "mcp__calc__add").as_deref(),
        Some("KB add description")
    );

    // 2. Strategy plus canary: a candidate, but nothing is accepted automatically.
    events.clear();
    f.handle.send(UiCommand::RunDrift {
        server: "calc".to_owned(),
        strategy: Strategy::FullRewrite,
    });
    wait_until(&mut f.handle, &mut events, "the canary report", |e| {
        canary_of(e).is_some()
    });
    let canary = canary_of(&events).expect("canary").clone();
    assert_eq!(canary.status, CanaryStatus::Pass, "{}", canary.summary);
    assert!(
        canary.can_accept && canary.decision == "promote",
        "{canary:?}"
    );
    assert!(!canary.probes.is_empty());
    assert!(
        last_status(&events, "calc").is_none(),
        "running a strategy does not touch the exposed tools"
    );
    assert!(
        f.ledger.lock().expect("ledger").spent_usd() > 0.0,
        "the model call was billed"
    );
    assert_eq!(f.mock.requests().len(), 1);
    assert!(
        f.mock.requests()[0]
            .instructions
            .contains("You maintain a Tool KB")
    );
    feed(&mut app, &events);
    assert!(matches!(
        app.drifts["calc"].phase,
        harness_tui::DriftPhase::Ready(_)
    ));

    // 3. Accept: the exposed description now matches the live server.
    events.clear();
    f.handle.send(UiCommand::AcceptDrift {
        server: "calc".to_owned(),
    });
    wait_until(&mut f.handle, &mut events, "the drift to clear", |e| {
        e.iter()
            .any(|e| matches!(e, UiEvent::DriftReady { report, .. } if report.is_empty()))
    });
    let after = last_status(&events, "calc").expect("status after accept");
    assert_eq!(
        exposed_description(after, "mcp__calc__add").as_deref(),
        Some("LIVE add description"),
        "accepting changed what the model sees"
    );
    assert_eq!(
        after.exposed.len(),
        4,
        "the accepted KB covers every live tool"
    );
    let summary = events.iter().rev().find_map(|e| match e {
        UiEvent::KbSummary(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        summary.expect("kb summary").servers,
        vec![("calc".to_owned(), 4)]
    );
    feed(&mut app, &events);
    assert!(app.drifts.is_empty());
    assert_eq!(app.status.drift_count, 0);
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn a_rejected_candidate_leaves_the_kb_and_the_exposed_tools_alone() {
    let (mut f, _) = drift_setup(|rewritten| vec![Scripted::text(rewritten)]);
    let mut events = Vec::new();
    wait_for_drift(&mut f, &mut events);
    events.clear();
    f.handle.send(UiCommand::RunDrift {
        server: "calc".to_owned(),
        strategy: Strategy::FullRewrite,
    });
    wait_until(&mut f.handle, &mut events, "the canary report", |e| {
        canary_of(e).is_some()
    });
    f.handle.send(UiCommand::RejectDrift {
        server: "calc".to_owned(),
    });
    wait_until(&mut f.handle, &mut events, "the rejection notice", |e| {
        notices(e)
            .iter()
            .any(|n| n.contains("rejected the candidate"))
    });
    // The candidate is gone: accepting now fails and nothing changed.
    events.clear();
    f.handle.send(UiCommand::AcceptDrift {
        server: "calc".to_owned(),
    });
    wait_until(&mut f.handle, &mut events, "a refusal", |e| {
        errors_contain(e, "no candidate KB")
    });
    assert!(last_status(&events, "calc").is_none());
    f.handle.shutdown(Duration::from_secs(5));
}

#[test]
fn a_no_update_run_cannot_be_accepted_and_a_garbage_rewrite_is_reported() {
    let (mut f, _) = drift_setup(|_| vec![Scripted::text("this is not a KB")]);
    let mut events = Vec::new();
    wait_for_drift(&mut f, &mut events);
    events.clear();

    f.handle.send(UiCommand::RunDrift {
        server: "calc".to_owned(),
        strategy: Strategy::NoUpdate,
    });
    wait_until(&mut f.handle, &mut events, "the no-update report", |e| {
        canary_of(e).is_some()
    });
    let canary = canary_of(&events).expect("canary").clone();
    assert!(!canary.can_accept, "{canary:?}");
    assert_eq!(canary.decision, "unchanged");
    assert_eq!(f.mock.call_count(), 0, "no update never calls the model");
    f.handle.send(UiCommand::AcceptDrift {
        server: "calc".to_owned(),
    });
    wait_until(&mut f.handle, &mut events, "a refusal", |e| {
        errors_contain(e, "cannot be accepted")
    });

    events.clear();
    f.handle.send(UiCommand::RunDrift {
        server: "calc".to_owned(),
        strategy: Strategy::FullRewrite,
    });
    wait_until(&mut f.handle, &mut events, "the failure", |e| {
        e.iter().any(|e| matches!(e, UiEvent::DriftFailed { .. }))
    });
    assert!(canary_of(&events).is_none());
    f.handle.send(UiCommand::RunDrift {
        server: "ghost".to_owned(),
        strategy: Strategy::NoUpdate,
    });
    wait_until(
        &mut f.handle,
        &mut events,
        "an unknown-server failure",
        |e| {
            e.iter().any(|e| matches!(e, UiEvent::DriftFailed { message, .. } if message.contains("not connected")))
        },
    );
    f.handle.shutdown(Duration::from_secs(5));
}

// -------------------------------------------------------------------------------------------
// Pure pieces
// -------------------------------------------------------------------------------------------

#[test]
fn split_agent_event_passes_every_plain_event_through_and_keeps_the_approval_sender() {
    use harness_tui::bridge::AgentEventSplit;
    let plain = [
        AgentEvent::TurnStarted,
        AgentEvent::TextDelta("t".to_owned()),
        AgentEvent::ToolCallStarted {
            call_id: "c".to_owned(),
            name: "n".to_owned(),
            arguments: json!({}),
        },
        AgentEvent::ToolCallFinished {
            call_id: "c".to_owned(),
            output: "o".to_owned(),
            is_error: false,
            duration_ms: 1,
        },
        AgentEvent::Usage {
            model_alias: "luna".to_owned(),
            usage: Default::default(),
            cost_usd: None,
        },
        AgentEvent::Notice("n".to_owned()),
        AgentEvent::Error("e".to_owned()),
        AgentEvent::TurnFinished {
            reason: StopReason::Completed,
        },
    ];
    for event in plain {
        assert!(matches!(
            split_agent_event(event),
            AgentEventSplit::Plain(_)
        ));
    }
    let (tx, mut rx) = oneshot::channel();
    let request = ApprovalRequest {
        id: 5,
        tool: "write_file".to_owned(),
        class: ApprovalClass::Write,
        summary: "s".to_owned(),
        diff: None,
    };
    match split_agent_event(AgentEvent::ApprovalRequested(request.clone(), tx)) {
        AgentEventSplit::Approval(got, reply) => {
            assert_eq!(got, request);
            reply.send(Decision::Session).expect("receiver alive");
            assert_eq!(rx.try_recv().expect("answer"), Decision::Session);
        }
        AgentEventSplit::Plain(other) => panic!("approval must not be plain: {other:?}"),
    }
}

#[test]
fn the_scripted_port_records_commands_replays_events_and_counts_shutdowns() {
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    handle.push(UiEvent::Notice("queued".to_owned()));
    handle.set_responder(|command| match command {
        UiCommand::SendUser(text) => vec![UiEvent::Notice(format!("echo {text}"))],
        _ => vec![],
    });
    port.send(UiCommand::SendUser("hi".to_owned()));
    port.send(UiCommand::Cancel);
    assert_eq!(
        handle.sent(),
        vec![UiCommand::SendUser("hi".to_owned()), UiCommand::Cancel]
    );
    assert_eq!(
        port.poll(),
        vec![
            UiEvent::Notice("queued".to_owned()),
            UiEvent::Notice("echo hi".to_owned())
        ]
    );
    assert!(port.poll().is_empty(), "events are delivered once");
    assert!(port.shutdown(Duration::from_secs(1)));
    assert_eq!(handle.shutdown_count(), 1);
}
