//! Reducer behaviour: keys, slash commands, approvals, Ctrl-C, streaming and drift.

mod common;

use std::collections::BTreeSet;

use common::*;
use harness_agent::approval::{ApprovalClass, ApprovalRequest, Decision};
use harness_agent::drift::DriftReport;
use harness_agent::events::StopReason;
use harness_agent::model::TokenUsage;
use harness_agent::registry::Effort;
use harness_core::diff::KbDiff;
use harness_core::{CanaryStatus, ChangeClass, ChangeKind, ChangeRecord, Strategy};
use harness_tui::theme::builtin;
use harness_tui::{
    AgentUiEvent, App, CTRL_C_WINDOW_TICKS, CanarySummary, CostSnapshot, DriftPhase, Effect, Entry,
    ExposedTool, KbSummary, McpServerStatus, McpState, Msg, Overlay, PickerKind, UiCommand,
    UiEvent,
};
use ratatui::crossterm::event::KeyCode;

fn notices(app: &App) -> Vec<&str> {
    app.transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Notice(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn errors(app: &App) -> Vec<&str> {
    app.transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Error(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn last_notice(app: &App) -> String {
    notices(app)
        .last()
        .map(|s| (*s).to_owned())
        .unwrap_or_default()
}

fn last_error(app: &App) -> String {
    errors(app)
        .last()
        .map(|s| (*s).to_owned())
        .unwrap_or_default()
}

fn request(id: u64, tool: &str, class: ApprovalClass) -> ApprovalRequest {
    ApprovalRequest {
        id,
        tool: tool.to_owned(),
        class,
        summary: format!("{tool} summary"),
        diff: None,
    }
}

fn open_approval(app: &mut App, id: u64) {
    ui_event(
        app,
        UiEvent::ApprovalNeeded {
            id,
            request: request(id, "write_file", ApprovalClass::Write),
        },
    );
}

fn connected(name: &str, managed: bool, live: usize) -> UiEvent {
    UiEvent::McpStatus(McpServerStatus {
        name: name.to_owned(),
        state: McpState::Connected,
        live_tools: live,
        managed,
        exposed: if managed {
            vec![ExposedTool {
                name: format!("mcp__{name}__add"),
                description: "KB add".to_owned(),
            }]
        } else {
            Vec::new()
        },
    })
}

fn change(tool: &str, field: &str) -> ChangeRecord {
    ChangeRecord {
        lineage_id: format!("calc::{tool}"),
        tool_id: tool.to_owned(),
        class: ChangeClass::Description,
        kind: ChangeKind::Modified,
        field_path: Some(field.to_owned()),
        old_hash: Some("a".repeat(16)),
        new_hash: Some("b".repeat(16)),
    }
}

fn report(server: &str, changes: Vec<ChangeRecord>) -> DriftReport {
    DriftReport {
        server: server.to_owned(),
        kb_cards: 2,
        live_tools: 2,
        diff: KbDiff {
            source_version: "kb-1".to_owned(),
            target_version: "live".to_owned(),
            affected_lineage_ids: changes
                .iter()
                .map(|c| c.lineage_id.clone())
                .collect::<BTreeSet<_>>(),
            changes,
        },
    }
}

fn canary(server: &str, status: CanaryStatus, can_accept: bool) -> CanarySummary {
    CanarySummary {
        server: server.to_owned(),
        strategy: Strategy::FullRewrite,
        status,
        summary: "canary".to_owned(),
        probes: Vec::new(),
        collateral_damage: false,
        decision: if can_accept {
            "promote".to_owned()
        } else {
            "blocked: canary failed".to_owned()
        },
        can_accept,
    }
}

fn app_with_drift() -> App {
    let mut app = fresh();
    ui_event(&mut app, connected("calc", true, 2));
    ui_event(
        &mut app,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: report("calc", vec![change("add", "description")]),
        },
    );
    app
}

// -------------------------------------------------------------------------------------------
// Sending, input, paste
// -------------------------------------------------------------------------------------------

#[test]
fn enter_sends_the_message_and_marks_the_app_busy() {
    let mut app = fresh();
    let effects = say(&mut app, "fix the bug");
    assert_eq!(
        effects,
        vec![Effect::Send(UiCommand::SendUser("fix the bug".to_owned()))]
    );
    assert_eq!(app.transcript, vec![Entry::User("fix the bug".to_owned())]);
    assert!(app.busy);
    assert!(app.input.is_empty());
}

#[test]
fn enter_is_ignored_while_busy_with_a_notice_and_the_draft_is_kept() {
    let mut app = fresh();
    say(&mut app, "first");
    type_text(&mut app, "second");
    let effects = app.update(key(KeyCode::Enter));
    assert!(effects.is_empty());
    assert_eq!(app.input.text(), "second");
    assert!(
        last_notice(&app).contains("A turn is running"),
        "{:?}",
        notices(&app)
    );
    assert_eq!(
        app.transcript
            .iter()
            .filter(|e| matches!(e, Entry::User(_)))
            .count(),
        1
    );
}

#[test]
fn empty_or_blank_input_sends_nothing() {
    let mut app = fresh();
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    type_text(&mut app, "   ");
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.transcript.is_empty());
    assert!(!app.busy);
}

#[test]
fn alt_enter_shift_enter_and_ctrl_j_insert_newlines() {
    let mut app = fresh();
    type_text(&mut app, "a");
    app.update(alt_enter());
    type_text(&mut app, "b");
    app.update(shift_enter());
    type_text(&mut app, "c");
    app.update(ctrl('j'));
    type_text(&mut app, "d");
    assert_eq!(app.input.text(), "a\nb\nc\nd");
    let effects = app.update(key(KeyCode::Enter));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SendUser("a\nb\nc\nd".to_owned())]
    );
}

#[test]
fn paste_lands_in_the_input_box_and_is_ignored_under_an_overlay() {
    let mut app = fresh();
    app.update(Msg::Paste("line one\r\nline two".to_owned()));
    assert_eq!(app.input.text(), "line one\nline two");
    app.update(ch('?'));
    assert!(
        app.overlay.is_none(),
        "? is a normal character once the input has text"
    );
    assert!(app.input.text().ends_with('?'));

    let mut app = fresh();
    open_approval(&mut app, 1);
    app.update(Msg::Paste("x".to_owned()));
    assert!(
        app.input.is_empty(),
        "paste must not reach the input behind a modal"
    );
}

#[test]
fn key_release_events_are_ignored() {
    let mut app = fresh();
    app.update(release('x'));
    assert!(app.input.is_empty());
}

#[test]
fn question_mark_on_an_empty_input_opens_help_and_esc_closes_it() {
    let mut app = fresh();
    app.update(ch('?'));
    assert_eq!(app.overlay, Some(Overlay::Help));
    app.update(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
    assert!(app.input.is_empty());
}

// -------------------------------------------------------------------------------------------
// Streaming and tool cards
// -------------------------------------------------------------------------------------------

#[test]
fn text_deltas_append_to_the_last_assistant_entry() {
    let mut app = fresh();
    agent_event(&mut app, AgentUiEvent::TurnStarted);
    agent_event(&mut app, AgentUiEvent::TextDelta("Hel".to_owned()));
    agent_event(&mut app, AgentUiEvent::TextDelta("lo".to_owned()));
    assert_eq!(
        app.transcript,
        vec![Entry::Assistant {
            text: "Hello".to_owned(),
            streaming: true
        }]
    );
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Completed,
        },
    );
    assert_eq!(
        app.transcript,
        vec![Entry::Assistant {
            text: "Hello".to_owned(),
            streaming: false
        }]
    );
    assert!(!app.busy);
}

#[test]
fn each_model_call_gets_its_own_assistant_entry() {
    let mut app = fresh();
    agent_event(&mut app, AgentUiEvent::TextDelta("first".to_owned()));
    agent_event(
        &mut app,
        AgentUiEvent::Usage {
            model_alias: "luna".to_owned(),
            usage: TokenUsage {
                input: Some(10),
                output_including_reasoning: Some(5),
                ..TokenUsage::default()
            },
            cost_usd: Some(0.0),
        },
    );
    agent_event(&mut app, AgentUiEvent::TextDelta("second".to_owned()));
    let texts: Vec<&str> = app
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "second"]);
    assert_eq!((app.status.tokens_in, app.status.tokens_out), (10, 5));
}

#[test]
fn tool_cards_follow_started_and_finished_events_and_tab_expands_the_focused_card() {
    let mut app = fresh();
    agent_event(&mut app, AgentUiEvent::TextDelta("reading".to_owned()));
    agent_event(
        &mut app,
        AgentUiEvent::ToolCallStarted {
            call_id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "a.txt"}),
        },
    );
    // The streaming entry is closed when a tool call starts.
    assert!(matches!(
        &app.transcript[0],
        Entry::Assistant {
            streaming: false,
            ..
        }
    ));
    let Entry::ToolCard(card) = &app.transcript[1] else {
        panic!("card expected")
    };
    assert_eq!(card.name, "read_file");
    assert_eq!(card.args, r#"{"path":"a.txt"}"#);
    assert!(card.output.is_none() && !card.expanded);

    agent_event(
        &mut app,
        AgentUiEvent::ToolCallFinished {
            call_id: "c1".to_owned(),
            output: "hello".to_owned(),
            is_error: false,
            duration_ms: 120,
        },
    );
    let Entry::ToolCard(card) = &app.transcript[1] else {
        panic!("card expected")
    };
    assert_eq!(card.output.as_deref(), Some("hello"));
    assert_eq!(card.duration_ms, Some(120));

    app.update(key(KeyCode::Tab));
    let Entry::ToolCard(card) = &app.transcript[1] else {
        panic!("card expected")
    };
    assert!(card.expanded);
    app.update(key(KeyCode::Tab));
    let Entry::ToolCard(card) = &app.transcript[1] else {
        panic!("card expected")
    };
    assert!(!card.expanded);
}

#[test]
fn back_tab_moves_the_focus_between_cards_and_tab_toggles_only_the_focused_one() {
    let mut app = fresh();
    for id in ["c1", "c2"] {
        agent_event(
            &mut app,
            AgentUiEvent::ToolCallStarted {
                call_id: id.to_owned(),
                name: "grep".to_owned(),
                arguments: serde_json::json!({}),
            },
        );
    }
    assert_eq!(app.focus, Some(1), "the newest card is focused");
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.focus, Some(0));
    app.update(key(KeyCode::Tab));
    let expanded: Vec<bool> = app
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::ToolCard(c) => Some(c.expanded),
            _ => None,
        })
        .collect();
    assert_eq!(expanded, vec![true, false]);
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.focus, Some(1), "focus wraps around");
}

#[test]
fn a_call_cancelled_before_it_started_still_gets_a_card() {
    let mut app = fresh();
    agent_event(
        &mut app,
        AgentUiEvent::ToolCallFinished {
            call_id: "never-started".to_owned(),
            output: "Cancelled by the user before this call ran.".to_owned(),
            is_error: true,
            duration_ms: 0,
        },
    );
    let Entry::ToolCard(card) = &app.transcript[0] else {
        panic!("card expected")
    };
    assert!(card.is_error);
    assert_eq!(
        card.output.as_deref(),
        Some("Cancelled by the user before this call ran.")
    );
}

#[test]
fn agent_notices_errors_and_stop_reasons_are_shown() {
    let mut app = fresh();
    agent_event(
        &mut app,
        AgentUiEvent::Notice("budget almost used".to_owned()),
    );
    agent_event(
        &mut app,
        AgentUiEvent::Error("model call failed: x".to_owned()),
    );
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Cancelled,
        },
    );
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::MaxSteps,
        },
    );
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Stalled,
        },
    );
    assert!(notices(&app).contains(&"budget almost used"));
    assert_eq!(last_error(&app), "model call failed: x");
    assert!(notices(&app).iter().any(|n| n.contains("cancelled")));
    assert!(notices(&app).iter().any(|n| n.contains("step limit")));
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("without finishing"))
    );
}

// -------------------------------------------------------------------------------------------
// Slash commands
// -------------------------------------------------------------------------------------------

#[test]
fn model_without_an_argument_opens_a_picker_that_sends_the_default_effort() {
    let mut app = fresh();
    let effects = say(&mut app, "/model");
    assert!(effects.is_empty());
    let Some(Overlay::Picker(view)) = &app.overlay else {
        panic!("picker expected")
    };
    let aliases: Vec<&str> = view.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(aliases, vec!["luna", "sol", "astra", "flash", "pro"]);
    assert_eq!(view.selected, 0, "the current model is preselected");
    assert!(
        view.items[1].detail.contains("default xhigh"),
        "{}",
        view.items[1].detail
    );

    app.update(key(KeyCode::Down));
    let effects = app.update(key(KeyCode::Enter));
    assert!(app.overlay.is_none());
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SetModel {
            alias: "sol".to_owned(),
            effort: Effort::Xhigh
        }]
    );
}

#[test]
fn the_picker_can_be_cancelled_and_clamps_at_both_ends() {
    let mut app = fresh();
    say(&mut app, "/model");
    app.update(key(KeyCode::Up));
    for _ in 0..10 {
        app.update(key(KeyCode::Down));
    }
    let Some(Overlay::Picker(view)) = &app.overlay else {
        panic!("picker expected")
    };
    assert_eq!(view.selected, 4);
    let effects = app.update(key(KeyCode::Esc));
    assert!(effects.is_empty() && app.overlay.is_none());
}

#[test]
fn model_with_an_alias_switches_directly_and_unknown_aliases_are_errors() {
    let mut app = fresh();
    let effects = say(&mut app, "/model sol");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SetModel {
            alias: "sol".to_owned(),
            effort: Effort::Xhigh
        }]
    );
    let effects = say(&mut app, "/model gpt-9");
    assert!(effects.is_empty());
    let error = last_error(&app);
    assert!(error.contains("unknown model alias"), "{error}");
    assert!(error.contains("luna, sol, astra, flash, pro"), "{error}");
}

#[test]
fn effort_is_validated_against_the_current_models_allowed_values() {
    let mut app = fresh();
    // luna accepts "none".
    let effects = say(&mut app, "/effort none");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SetModel {
            alias: "luna".to_owned(),
            effort: Effort::None
        }]
    );
    // After switching to sol, "none" is not allowed.
    ui_event(
        &mut app,
        UiEvent::ModelChanged {
            alias: "sol".to_owned(),
            effort: Effort::Xhigh,
        },
    );
    assert_eq!(app.status.model_alias, "sol");
    let effects = say(&mut app, "/effort none");
    assert!(effects.is_empty());
    let error = last_error(&app);
    assert!(
        error.contains("model sol does not accept effort"),
        "{error}"
    );
    assert!(error.contains("low, medium, high, xhigh, max"), "{error}");
    // Gemini models allow only low, medium and high.
    ui_event(
        &mut app,
        UiEvent::ModelChanged {
            alias: "flash".to_owned(),
            effort: Effort::Medium,
        },
    );
    say(&mut app, "/effort max");
    assert!(
        last_error(&app).contains("low, medium, high"),
        "{}",
        last_error(&app)
    );
    // Names are case-insensitive; garbage is an error too.
    let effects = say(&mut app, "/effort HIGH");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SetModel {
            alias: "flash".to_owned(),
            effort: Effort::High
        }]
    );
    assert!(say(&mut app, "/effort loud").is_empty());
    assert!(
        last_error(&app).contains("allowed:"),
        "{}",
        last_error(&app)
    );
}

#[test]
fn model_changes_are_refused_while_a_turn_runs_but_read_only_commands_work() {
    let mut app = fresh();
    say(&mut app, "work");
    assert!(app.busy);
    assert!(say(&mut app, "/model sol").is_empty());
    assert!(
        last_notice(&app).contains("not available while a turn"),
        "{}",
        last_notice(&app)
    );
    assert!(say(&mut app, "/effort high").is_empty());
    assert!(app.overlay.is_none());
    say(&mut app, "/model");
    assert!(app.overlay.is_none(), "no picker while busy");
    say(&mut app, "/cost");
    assert!(
        last_notice(&app).starts_with("Spent $"),
        "{}",
        last_notice(&app)
    );
    say(&mut app, "/help");
    assert_eq!(app.overlay, Some(Overlay::Help));
}

#[test]
fn mcp_lists_servers_with_state_tool_counts_and_management() {
    let mut app = App::new(app_init(
        "luna",
        Effort::Max,
        &["calc", "docs", "down", "broken"],
    ));
    say(&mut app, "/mcp");
    assert!(
        last_notice(&app).contains("calc: disconnected"),
        "{}",
        last_notice(&app)
    );
    ui_event(&mut app, connected("calc", true, 4));
    ui_event(&mut app, connected("docs", false, 3));
    ui_event(
        &mut app,
        UiEvent::McpStatus(McpServerStatus {
            name: "broken".to_owned(),
            state: McpState::Failed("no such program".to_owned()),
            live_tools: 0,
            managed: false,
            exposed: Vec::new(),
        }),
    );
    say(&mut app, "/mcp");
    let list = last_notice(&app);
    assert!(
        list.contains("calc: connected, 4 live tool(s), managed by the KB (1 exposed)"),
        "{list}"
    );
    assert!(
        list.contains("docs: connected, 3 live tool(s), not managed by the KB: no tools exposed"),
        "{list}"
    );
    assert!(list.contains("/kb adopt docs"), "{list}");
    assert!(list.contains("down: disconnected"), "{list}");
    assert!(list.contains("broken: failed (no such program)"), "{list}");
    assert_eq!((app.status.mcp_up, app.status.mcp_total), (2, 4));
}

#[test]
fn mcp_without_servers_says_so() {
    let mut app = fresh();
    say(&mut app, "/mcp");
    assert!(
        last_notice(&app).contains("No MCP servers configured"),
        "{}",
        last_notice(&app)
    );
}

#[test]
fn mcp_connect_and_disconnect_validate_the_name() {
    let mut app = App::new(app_init("luna", Effort::Max, &["calc"]));
    let effects = say(&mut app, "/mcp connect calc");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::ConnectMcp("calc".to_owned())]
    );
    let effects = say(&mut app, "/mcp disconnect calc");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::DisconnectMcp("calc".to_owned())]
    );
    assert!(say(&mut app, "/mcp connect ghost").is_empty());
    let error = last_error(&app);
    assert!(
        error.contains("no MCP server named \"ghost\"") && error.contains("calc"),
        "{error}"
    );
    say(&mut app, "work");
    assert!(
        say(&mut app, "/mcp connect calc").is_empty(),
        "refused while busy"
    );
}

#[test]
fn kb_shows_id_version_and_cards_per_server() {
    let mut app = fresh();
    say(&mut app, "/kb");
    assert!(
        last_notice(&app).contains("No KB information"),
        "{}",
        last_notice(&app)
    );
    ui_event(
        &mut app,
        UiEvent::KbSummary(KbSummary {
            kb_id: "kb-1".to_owned(),
            server_version: "v7".to_owned(),
            servers: vec![("calc".to_owned(), 3), ("docs".to_owned(), 5)],
        }),
    );
    say(&mut app, "/kb");
    let text = last_notice(&app);
    assert!(text.contains("KB kb-1 (version v7)"), "{text}");
    assert!(
        text.contains("calc: 3 card(s)") && text.contains("docs: 5 card(s)"),
        "{text}"
    );
    ui_event(
        &mut app,
        UiEvent::KbSummary(KbSummary {
            kb_id: "session".to_owned(),
            server_version: "live".to_owned(),
            servers: vec![],
        }),
    );
    say(&mut app, "/kb");
    assert!(
        last_notice(&app).contains("every MCP server is unmanaged"),
        "{}",
        last_notice(&app)
    );
}

#[test]
fn kb_adopt_asks_for_confirmation_before_sending() {
    let mut app = App::new(app_init("luna", Effort::Max, &["docs", "calc"]));
    // Not connected: refused.
    assert!(say(&mut app, "/kb adopt docs").is_empty());
    assert!(
        last_error(&app).contains("not connected"),
        "{}",
        last_error(&app)
    );
    assert!(say(&mut app, "/kb adopt ghost").is_empty());
    assert!(
        last_error(&app).contains("no MCP server named"),
        "{}",
        last_error(&app)
    );

    ui_event(&mut app, connected("docs", false, 3));
    ui_event(&mut app, connected("calc", true, 2));
    assert!(say(&mut app, "/kb adopt calc").is_empty());
    assert!(
        last_error(&app).contains("already managed"),
        "{}",
        last_error(&app)
    );

    let effects = say(&mut app, "/kb adopt docs");
    assert!(
        effects.is_empty(),
        "nothing is sent before the user confirms"
    );
    let Some(Overlay::Confirm(view)) = &app.overlay else {
        panic!("confirm expected")
    };
    assert!(
        view.prompt.contains("3 live tool(s) of docs"),
        "{}",
        view.prompt
    );
    // Enter does not confirm.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));
    let effects = app.update(ch('y'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::AdoptServer("docs".to_owned())]
    );
    assert!(app.overlay.is_none());

    say(&mut app, "/kb adopt docs");
    let effects = app.update(ch('n'));
    assert!(effects.is_empty() && app.overlay.is_none());
}

#[test]
fn drift_cost_clear_help_and_quit_commands() {
    let mut app = fresh();
    say(&mut app, "/drift");
    assert!(matches!(app.overlay, Some(Overlay::Drift(_))));
    app.update(key(KeyCode::Esc));

    ui_event(
        &mut app,
        UiEvent::Cost(CostSnapshot {
            spent_usd: 0.1234,
            unreconciled_usd: 0.5,
            remaining_usd: 4.3766,
            cap_usd: 5.0,
        }),
    );
    say(&mut app, "/cost");
    let cost = last_notice(&app);
    assert!(cost.contains("Spent $0.1234 of $5.00"), "{cost}");
    assert!(cost.contains("unreconciled $0.5000"), "{cost}");
    assert!(cost.contains("remaining $4.3766"), "{cost}");

    assert!(!app.transcript.is_empty());
    say(&mut app, "/clear");
    assert!(app.transcript.is_empty());

    say(&mut app, "/help");
    assert_eq!(app.overlay, Some(Overlay::Help));
    app.update(key(KeyCode::Esc));

    let effects = say(&mut app, "/quit");
    assert_eq!(effects, vec![Effect::Quit]);
    assert!(app.should_quit());
}

#[test]
fn unknown_commands_are_errors_and_paths_are_ordinary_messages() {
    let mut app = fresh();
    assert!(say(&mut app, "/frobnicate").is_empty());
    assert!(
        last_error(&app).contains("unknown command /frobnicate"),
        "{}",
        last_error(&app)
    );
    assert!(!app.busy);
    say(&mut app, "/model a b");
    assert!(
        last_error(&app).contains("usage: /model"),
        "{}",
        last_error(&app)
    );
    let effects = say(&mut app, "/etc/hosts is missing");
    assert_eq!(
        sent(&effects),
        vec![UiCommand::SendUser("/etc/hosts is missing".to_owned())]
    );
}

// -------------------------------------------------------------------------------------------
// Approval
// -------------------------------------------------------------------------------------------

#[test]
fn approval_keys_map_to_decisions() {
    for (code, decision) in [
        (ch('y'), Decision::Once),
        (ch('a'), Decision::Session),
        (ch('n'), Decision::Deny),
        (key(KeyCode::Esc), Decision::Deny),
    ] {
        let mut app = fresh();
        open_approval(&mut app, 7);
        let effects = app.update(code);
        assert_eq!(sent(&effects), vec![UiCommand::Approve { id: 7, decision }]);
        assert!(app.overlay.is_none());
    }
}

#[test]
fn nothing_else_answers_an_approval() {
    let mut app = fresh();
    say(&mut app, "go");
    open_approval(&mut app, 3);
    for msg in [
        key(KeyCode::Enter),
        key(KeyCode::Tab),
        key(KeyCode::Right),
        ch('x'),
        ch('Y'),
        ch(' '),
        ch('q'),
        ctrl('c'),
        ctrl('y'),
        Msg::Tick,
    ] {
        let effects = app.update(msg.clone());
        assert!(effects.is_empty(), "{msg:?} must not act on an approval");
        assert!(
            matches!(app.overlay, Some(Overlay::Approval(_))),
            "{msg:?} must not close the approval"
        );
    }
    assert!(
        app.busy,
        "Ctrl-C does not leave the turn or quit while an approval is open"
    );
    assert!(!app.should_quit());
}

#[test]
fn queued_approvals_are_shown_one_at_a_time_in_order() {
    let mut app = fresh();
    open_approval(&mut app, 1);
    open_approval(&mut app, 2);
    let Some(Overlay::Approval(view)) = &app.overlay else {
        panic!("approval expected")
    };
    assert_eq!(view.id, 1);
    let effects = app.update(ch('n'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::Approve {
            id: 1,
            decision: Decision::Deny
        }]
    );
    let Some(Overlay::Approval(view)) = &app.overlay else {
        panic!("second approval expected")
    };
    assert_eq!(view.id, 2);
    app.update(ch('y'));
    assert!(app.overlay.is_none());
}

#[test]
fn an_approval_replaces_other_overlays_and_ends_with_the_turn() {
    let mut app = fresh();
    say(&mut app, "/help");
    open_approval(&mut app, 4);
    assert!(matches!(app.overlay, Some(Overlay::Approval(_))));
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Cancelled,
        },
    );
    assert!(
        app.overlay.is_none(),
        "a finished turn leaves nothing to approve"
    );
}

// -------------------------------------------------------------------------------------------
// Ctrl-C and scrolling
// -------------------------------------------------------------------------------------------

#[test]
fn ctrl_c_cancels_a_running_turn_instead_of_quitting() {
    let mut app = fresh();
    say(&mut app, "work");
    let effects = app.update(ctrl('c'));
    assert_eq!(effects, vec![Effect::Send(UiCommand::Cancel)]);
    assert!(!app.should_quit());
    let effects = app.update(ctrl('c'));
    assert_eq!(
        effects,
        vec![Effect::Send(UiCommand::Cancel)],
        "still cancelling, never quitting mid-turn"
    );
    assert!(!app.should_quit());
}

#[test]
fn ctrl_c_twice_within_the_window_quits_when_idle() {
    let mut app = fresh();
    assert!(app.update(ctrl('c')).is_empty());
    assert!(!app.should_quit());
    assert!(
        last_notice(&app).contains("Ctrl-C again"),
        "{}",
        last_notice(&app)
    );
    let effects = app.update(ctrl('c'));
    assert_eq!(effects, vec![Effect::Quit]);
    assert!(app.should_quit());
}

#[test]
fn the_double_ctrl_c_window_expires_after_one_second_of_ticks() {
    let mut app = fresh();
    app.update(ctrl('c'));
    for _ in 0..CTRL_C_WINDOW_TICKS {
        app.update(Msg::Tick);
    }
    assert!(
        app.update(ctrl('c')).is_empty(),
        "a late second press only re-arms"
    );
    assert!(!app.should_quit());
    app.update(Msg::Tick);
    assert!(app.update(ctrl('c')).contains(&Effect::Quit));
}

#[test]
fn page_keys_scroll_and_sending_returns_to_the_bottom() {
    let mut app = fresh();
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::PageUp));
    assert_eq!(app.scroll.from_bottom, 20);
    app.update(key(KeyCode::PageDown));
    assert_eq!(app.scroll.from_bottom, 10);
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    assert_eq!(app.scroll.from_bottom, 0, "never below the bottom");
    app.update(key(KeyCode::PageUp));
    say(&mut app, "hello");
    assert_eq!(app.scroll.from_bottom, 0);
}

// -------------------------------------------------------------------------------------------
// Status, MCP and drift events
// -------------------------------------------------------------------------------------------

#[test]
fn model_cost_and_usage_events_update_the_status_line() {
    let mut app = fresh();
    assert_eq!(
        (app.status.model_alias.as_str(), app.status.effort),
        ("luna", Effort::Max)
    );
    ui_event(
        &mut app,
        UiEvent::ModelChanged {
            alias: "luna".to_owned(),
            effort: Effort::Max,
        },
    );
    assert!(
        notices(&app).is_empty(),
        "an unchanged model is not announced"
    );
    ui_event(
        &mut app,
        UiEvent::ModelChanged {
            alias: "sol".to_owned(),
            effort: Effort::High,
        },
    );
    assert!(last_notice(&app).contains("sol") && last_notice(&app).contains("high"));
    assert_eq!(
        (app.status.model_alias.as_str(), app.status.effort),
        ("sol", Effort::High)
    );
    ui_event(
        &mut app,
        UiEvent::Cost(CostSnapshot {
            spent_usd: 1.5,
            unreconciled_usd: 0.0,
            remaining_usd: 3.5,
            cap_usd: 5.0,
        }),
    );
    assert_eq!((app.status.spent_usd, app.status.cap_usd), (1.5, 5.0));
}

#[test]
fn drift_reports_set_the_count_and_empty_reports_clear_it() {
    let mut app = app_with_drift();
    assert_eq!(app.status.drift_count, 1);
    assert!(
        last_notice(&app).contains("Drift on calc: 1 change(s)"),
        "{}",
        last_notice(&app)
    );
    let notices_before = notices(&app).len();
    // The same drift again is not announced twice.
    ui_event(
        &mut app,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: report("calc", vec![change("add", "description")]),
        },
    );
    assert_eq!(notices(&app).len(), notices_before);
    ui_event(
        &mut app,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: report("calc", vec![]),
        },
    );
    assert_eq!(app.status.drift_count, 0);
    assert!(app.drifts.is_empty());
}

#[test]
fn disconnecting_a_server_drops_its_drift() {
    let mut app = app_with_drift();
    ui_event(
        &mut app,
        UiEvent::McpStatus(McpServerStatus {
            name: "calc".to_owned(),
            state: McpState::Disconnected,
            live_tools: 0,
            managed: true,
            exposed: Vec::new(),
        }),
    );
    assert!(app.drifts.is_empty());
    assert_eq!(app.status.drift_count, 0);
}

#[test]
fn drift_overlay_runs_strategies_and_accepts_only_a_passing_candidate() {
    let mut app = app_with_drift();
    say(&mut app, "/drift");

    // Accept and reject do nothing before a candidate exists.
    assert!(app.update(ch('a')).is_empty());
    assert!(app.update(ch('r')).is_empty());

    for (key_char, strategy) in [
        ('n', Strategy::NoUpdate),
        ('f', Strategy::FullRewrite),
        ('s', Strategy::SelectivePatch),
    ] {
        let mut app = app_with_drift();
        say(&mut app, "/drift");
        let effects = app.update(ch(key_char));
        assert_eq!(
            sent(&effects),
            vec![UiCommand::RunDrift {
                server: "calc".to_owned(),
                strategy
            }]
        );
        assert_eq!(app.drifts["calc"].phase, DriftPhase::Running(strategy));
        // While it runs, nothing else can be sent (messages are blocked).
        assert!(app.blocked());
        assert!(
            app.update(ch('f')).is_empty(),
            "no second run while one is in progress"
        );
    }

    // A failing canary can be rejected but not accepted.
    let mut failing = app_with_drift();
    say(&mut failing, "/drift");
    failing.update(ch('f'));
    ui_event(
        &mut failing,
        UiEvent::CanaryReady(canary("calc", CanaryStatus::Fail, false)),
    );
    assert!(matches!(failing.drifts["calc"].phase, DriftPhase::Ready(_)));
    assert!(
        failing.update(ch('a')).is_empty(),
        "a blocked candidate cannot be accepted"
    );
    let effects = failing.update(ch('r'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::RejectDrift {
            server: "calc".to_owned()
        }]
    );
    assert_eq!(failing.drifts["calc"].phase, DriftPhase::Idle);

    // A passing canary can be accepted.
    let mut passing = app_with_drift();
    say(&mut passing, "/drift");
    passing.update(ch('f'));
    ui_event(
        &mut passing,
        UiEvent::CanaryReady(canary("calc", CanaryStatus::Pass, true)),
    );
    let effects = passing.update(ch('a'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::AcceptDrift {
            server: "calc".to_owned()
        }]
    );
    // The bridge answers with an empty report once the KB matches the live server.
    ui_event(
        &mut passing,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: report("calc", vec![]),
        },
    );
    assert!(passing.drifts.is_empty());
}

#[test]
fn drift_runs_are_refused_while_a_turn_runs_and_failures_reset_the_phase() {
    let mut app = app_with_drift();
    say(&mut app, "/drift");
    app.update(key(KeyCode::Esc));
    say(&mut app, "work");
    say(&mut app, "/drift");
    assert!(app.update(ch('f')).is_empty());
    assert!(
        last_notice(&app).contains("not available while"),
        "{}",
        last_notice(&app)
    );
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Completed,
        },
    );
    let effects = app.update(ch('f'));
    assert_eq!(effects.len(), 1);
    ui_event(
        &mut app,
        UiEvent::DriftFailed {
            server: "calc".to_owned(),
            message: "model call failed".to_owned(),
        },
    );
    assert_eq!(app.drifts["calc"].phase, DriftPhase::Idle);
    assert!(
        last_error(&app).contains("drift run for calc failed: model call failed"),
        "{}",
        last_error(&app)
    );
}

#[test]
fn drift_overlay_navigation_clamps_between_servers() {
    let mut app = fresh();
    for server in ["alpha", "beta"] {
        ui_event(&mut app, connected(server, true, 2));
        ui_event(
            &mut app,
            UiEvent::DriftReady {
                server: server.to_owned(),
                report: report(server, vec![change("add", "description")]),
            },
        );
    }
    say(&mut app, "/drift");
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    let effects = app.update(ch('n'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::RunDrift {
            server: "beta".to_owned(),
            strategy: Strategy::NoUpdate
        }],
        "selection stops at the last server"
    );

    app.update(Msg::Ui(UiEvent::DriftFailed {
        server: "beta".to_owned(),
        message: "x".to_owned(),
    }));
    app.update(key(KeyCode::Up));
    app.update(key(KeyCode::Up));
    let effects = app.update(ch('f'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::RunDrift {
            server: "alpha".to_owned(),
            strategy: Strategy::FullRewrite
        }],
        "selection stops at the first server"
    );
}

#[test]
fn fatal_events_stop_further_sending() {
    let mut app = fresh();
    say(&mut app, "work");
    ui_event(
        &mut app,
        UiEvent::Fatal("the agent runtime panicked".to_owned()),
    );
    assert!(!app.busy);
    assert!(
        last_error(&app).contains("agent runtime stopped: the agent runtime panicked"),
        "{}",
        last_error(&app)
    );
    let effects = say(&mut app, "anyone there?");
    assert!(effects.is_empty());
    assert!(
        last_notice(&app).contains("restart"),
        "{}",
        last_notice(&app)
    );
}

#[test]
fn error_messages_are_shown_in_the_transcript() {
    let mut app = fresh();
    app.update(Msg::Error("something broke".to_owned()));
    assert_eq!(last_error(&app), "something broke");
    ui_event(&mut app, UiEvent::Error("runtime says no".to_owned()));
    assert_eq!(last_error(&app), "runtime says no");
    ui_event(&mut app, UiEvent::Notice("fyi".to_owned()));
    assert_eq!(last_notice(&app), "fyi");
}

fn builtin_bg(slug: &str) -> harness_tui::theme::Rgb {
    builtin::theme(builtin::find(slug).expect("built-in exists")).bg
}

#[test]
fn theme_picker_lists_every_built_in_with_the_current_one_selected() {
    let mut app = fresh();
    let effects = say(&mut app, "/theme");
    assert!(effects.is_empty());
    let Some(Overlay::Picker(view)) = &app.overlay else {
        panic!("picker expected")
    };
    assert_eq!(view.kind, PickerKind::Theme);
    assert_eq!(view.items.len(), 20);
    assert_eq!(view.items[view.selected].value, "tokyo-night");
    assert!(
        view.items
            .iter()
            .any(|i| i.label == "gruvbox-dark" && i.detail == "dark")
    );
    assert!(
        view.items
            .iter()
            .any(|i| i.label == "solarized-light" && i.detail == "light")
    );
}

#[test]
fn theme_picker_previews_moves_and_escape_restores_the_exact_theme() {
    let mut app = fresh();
    let before = app.theme.clone();
    say(&mut app, "/theme");
    app.update(key(KeyCode::Down));
    assert_eq!(app.theme.bg, builtin_bg("gruvbox-dark"));
    app.update(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
    assert_eq!(app.theme, before);
    assert_eq!(app.theme_spec.label(), "tokyo-night");
}

#[test]
fn theme_picker_enter_keeps_the_previewed_theme() {
    let mut app = fresh();
    say(&mut app, "/theme");
    app.update(key(KeyCode::Down));
    let effects = app.update(key(KeyCode::Enter));
    assert!(effects.is_empty() && app.overlay.is_none());
    assert_eq!(app.theme.bg, builtin_bg("gruvbox-dark"));
    assert_eq!(app.theme_spec.label(), "gruvbox-dark");
}

#[test]
fn theme_with_a_name_switches_and_unknown_or_extra_arguments_are_errors() {
    let mut app = fresh();
    say(&mut app, "/theme nord");
    assert_eq!(app.theme.bg, builtin_bg("nord"));
    assert_eq!(app.theme_spec.label(), "nord");

    say(&mut app, "/theme nope");
    let error = last_error(&app);
    assert!(
        error.contains("nope") && error.contains("gruvbox-dark-hard"),
        "{error}"
    );

    say(&mut app, "/theme a b");
    assert_eq!(last_error(&app), "usage: /theme [name]");
    assert_eq!(
        app.theme_spec.label(),
        "nord",
        "failed commands change nothing"
    );
}

#[test]
fn theme_works_while_a_turn_is_running() {
    let mut app = fresh();
    say(&mut app, "hello");
    say(&mut app, "/theme nord");
    assert!(!notices(&app).iter().any(|n| n.contains("not available")));
    assert_eq!(app.theme_spec.label(), "nord");
}
