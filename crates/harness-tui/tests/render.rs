//! `TestBackend` render tests: layout, text, colours and small terminals.

mod common;

use std::collections::BTreeSet;

use common::*;
use harness_agent::approval::{ApprovalClass, ApprovalRequest};
use harness_agent::drift::DriftReport;
use harness_agent::events::StopReason;
use harness_agent::model::TokenUsage;
use harness_agent::registry::Effort;
use harness_core::diff::KbDiff;
use harness_core::{CanaryStatus, ChangeClass, ChangeKind, ChangeRecord, Strategy};
use harness_tui::{AgentUiEvent, App, CanarySummary, CostSnapshot, Msg, ProbeLine, UiEvent, ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier};

fn started(app: &mut App, id: &str, name: &str, args: serde_json::Value) {
    agent_event(
        app,
        AgentUiEvent::ToolCallStarted {
            call_id: id.to_owned(),
            name: name.to_owned(),
            arguments: args,
        },
    );
}

fn finished(app: &mut App, id: &str, output: &str, is_error: bool, ms: u64) {
    agent_event(
        app,
        AgentUiEvent::ToolCallFinished {
            call_id: id.to_owned(),
            output: output.to_owned(),
            is_error,
            duration_ms: ms,
        },
    );
}

fn approval(app: &mut App, request: ApprovalRequest) {
    ui_event(
        app,
        UiEvent::ApprovalNeeded {
            id: request.id,
            request,
        },
    );
}

fn change(tool: &str, field: &str) -> ChangeRecord {
    ChangeRecord {
        lineage_id: format!("calc::{tool}"),
        tool_id: tool.to_owned(),
        class: ChangeClass::Description,
        kind: ChangeKind::Modified,
        field_path: Some(field.to_owned()),
        old_hash: None,
        new_hash: None,
    }
}

fn drift_report(changes: Vec<ChangeRecord>) -> DriftReport {
    DriftReport {
        server: "calc".to_owned(),
        kb_cards: 2,
        live_tools: 3,
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

fn canary(status: CanaryStatus, can_accept: bool) -> CanarySummary {
    CanarySummary {
        server: "calc".to_owned(),
        strategy: Strategy::FullRewrite,
        status,
        summary: String::new(),
        probes: vec![
            ProbeLine {
                probe_id: Some("probe-add".to_owned()),
                tool_id: Some("add".to_owned()),
                is_control: false,
                status: CanaryStatus::Pass,
                expected: Some("sum".to_owned()),
                actual: Some("returned 42".to_owned()),
            },
            ProbeLine {
                probe_id: Some("probe-shout".to_owned()),
                tool_id: Some("shout".to_owned()),
                is_control: true,
                status: if can_accept {
                    CanaryStatus::Pass
                } else {
                    CanaryStatus::Fail
                },
                expected: Some("HELLO".to_owned()),
                actual: Some("returned hello".to_owned()),
            },
        ],
        collateral_damage: !can_accept,
        decision: if can_accept {
            "promote".to_owned()
        } else {
            "blocked: control probe failed".to_owned()
        },
        can_accept,
    }
}

fn drift_app(changes: Vec<ChangeRecord>) -> App {
    let mut app = fresh();
    ui_event(
        &mut app,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: drift_report(changes),
        },
    );
    say(&mut app, "/drift");
    app
}

// -------------------------------------------------------------------------------------------

#[test]
fn an_empty_session_shows_the_banner_input_and_status_bar() {
    let app = fresh();
    let t = text(&draw(&app, 100, 24));
    assert!(t.contains("mcpe"), "{t}");
    assert!(t.contains("/work/project"), "{t}");
    assert!(t.contains("coding agent · luna · effort max"), "{t}");
    assert!(
        t.contains("/help") && t.contains("/model") && t.contains("Alt+Enter"),
        "{t}"
    );
    assert!(t.contains("❯ Ask anything, or type / for commands"), "{t}");
    assert!(t.contains("Enter send · Alt+Enter newline"), "{t}");
    let status = t.lines().last().expect("status row");
    assert!(status.contains("luna/max"), "{status}");
    assert!(status.contains("$0.0000 / $5.00"), "{status}");
    assert!(status.contains("MCP 0/0"), "{status}");
    assert!(status.contains("drift 0"), "{status}");
}

#[test]
fn the_status_bar_shows_model_tokens_spend_servers_and_drift() {
    let mut app = App::new(app_init("sol", Effort::Xhigh, &["calc", "docs"]));
    agent_event(
        &mut app,
        AgentUiEvent::Usage {
            model_alias: "sol".to_owned(),
            usage: TokenUsage {
                input: Some(1_234),
                output_including_reasoning: Some(340),
                ..TokenUsage::default()
            },
            cost_usd: Some(0.01),
        },
    );
    ui_event(
        &mut app,
        UiEvent::Cost(CostSnapshot {
            spent_usd: 0.0123,
            unreconciled_usd: 0.0,
            remaining_usd: 4.9877,
            cap_usd: 5.0,
        }),
    );
    ui_event(
        &mut app,
        UiEvent::McpStatus(harness_tui::McpServerStatus {
            name: "calc".to_owned(),
            state: harness_tui::McpState::Connected,
            live_tools: 3,
            managed: true,
            exposed: vec![],
        }),
    );
    ui_event(
        &mut app,
        UiEvent::DriftReady {
            server: "calc".to_owned(),
            report: drift_report(vec![change("add", "description")]),
        },
    );
    settle(&mut app);
    let buffer = draw(&app, 100, 24);
    let t = text(&buffer);
    let status = t.lines().last().expect("status row");
    assert!(status.contains("sol/xhigh"), "{status}");
    assert!(status.contains("tokens in 1.2k out 340"), "{status}");
    assert!(status.contains("$0.0123 / $5.00"), "{status}");
    assert!(status.contains("MCP 1/2"), "{status}");
    assert!(status.contains("drift 1"), "{status}");
    let (x, y) = find(&buffer, "drift 1").expect("drift segment");
    assert_eq!(
        buffer[(x, y)].bg,
        app.theme.resolve(app.theme.warning),
        "a non-zero drift count is a warning chip"
    );
}

#[test]
fn a_streamed_reply_shows_the_cursor_block_until_the_turn_finishes() {
    let mut app = fresh();
    say(&mut app, "hello agent");
    agent_event(&mut app, AgentUiEvent::TextDelta("Hello, ".to_owned()));
    agent_event(
        &mut app,
        AgentUiEvent::TextDelta("how can I help".to_owned()),
    );
    let streaming = text(&draw(&app, 80, 24));
    assert!(streaming.contains("▎ hello agent"), "{streaming}");
    assert!(streaming.contains("Hello, how can I help█"), "{streaming}");
    assert!(
        streaming.contains("working…"),
        "the input box shows that a turn is running:\n{streaming}"
    );

    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: StopReason::Completed,
        },
    );
    let done = text(&draw(&app, 80, 24));
    assert!(done.contains("Hello, how can I help"), "{done}");
    assert!(
        !done.contains('█'),
        "no cursor after the reply is complete:\n{done}"
    );
    assert!(!done.contains("working…"), "{done}");
}

#[test]
fn notices_and_errors_are_styled_differently() {
    let mut app = fresh();
    ui_event(&mut app, UiEvent::Notice("just so you know".to_owned()));
    ui_event(&mut app, UiEvent::Error("it broke".to_owned()));
    let buffer = draw(&app, 80, 24);
    let (nx, ny) = find(&buffer, "just so you know").expect("notice");
    assert_eq!(buffer[(nx, ny)].fg, app.theme.resolve(app.theme.muted));
    let (ex, ey) = find(&buffer, "error: it broke").expect("error");
    assert_eq!(buffer[(ex, ey)].fg, app.theme.resolve(app.theme.error));
}

#[test]
fn collapsed_tool_cards_show_name_arguments_status_and_duration() {
    let mut app = fresh();
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "src/main.rs"}),
    );
    finished(&mut app, "c1", "fn main() {}", false, 120);
    started(
        &mut app,
        "c2",
        "run_shell",
        serde_json::json!({"command": "false"}),
    );
    finished(&mut app, "c2", "exit status 1", true, 8);
    started(
        &mut app,
        "c3",
        "write_file",
        serde_json::json!({"path": "x"}),
    );
    finished(&mut app, "c3", "The user denied this tool call.", true, 2);
    started(
        &mut app,
        "c4",
        "grep",
        serde_json::json!({"pattern": "todo"}),
    );
    let buffer = draw(&app, 100, 24);
    let t = text(&buffer);
    assert!(
        t.contains(r#"▸ read_file({"path":"src/main.rs"})  ✓ 120ms"#),
        "{t}"
    );
    assert!(
        t.contains(r#"▸ run_shell({"command":"false"})  ✗ 8ms"#),
        "{t}"
    );
    assert!(
        t.contains(r#"▸ write_file({"path":"x"})  ✗ denied"#),
        "a denial is visible on the card:\n{t}"
    );
    assert!(
        t.contains(r#"▸ grep({"pattern":"todo"})  ⠋ running"#),
        "a running call shows a spinner:\n{t}"
    );
    let (x, y) = find(&buffer, "✓ 120ms").expect("ok status");
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.success));
    let (x, y) = find(&buffer, "✗ 8ms").expect("error status");
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.error));
}

#[test]
fn long_arguments_are_truncated_so_the_status_stays_visible() {
    let mut app = fresh();
    started(
        &mut app,
        "c1",
        "write_file",
        serde_json::json!({"content": "x".repeat(300)}),
    );
    finished(&mut app, "c1", "ok", false, 5);
    let t = text(&draw(&app, 60, 12));
    let line = t
        .lines()
        .find(|l| l.contains("write_file("))
        .expect("card line");
    assert!(line.contains('…'), "{line}");
    assert!(line.ends_with("✓ 5ms"), "{line}");
    assert!(line.chars().count() <= 60, "{line}");
}

#[test]
fn expanded_tool_cards_pretty_print_args_and_cap_the_output_at_forty_lines() {
    let mut app = fresh();
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "a.txt", "limit": 3}),
    );
    let output: String = (1..=50).map(|i| format!("line {i}\n")).collect();
    finished(&mut app, "c1", &output, false, 33);
    app.update(key(KeyCode::Tab));
    let t = text(&draw(&app, 80, 80));
    assert!(t.contains("▾ read_file  ✓ 33ms"), "{t}");
    assert!(t.contains("args:"), "{t}");
    assert!(
        t.contains("    \"path\": \"a.txt\""),
        "args are pretty printed:\n{t}"
    );
    assert!(t.contains("output:"), "{t}");
    assert!(t.contains("line 40"), "{t}");
    assert!(
        !t.contains("line 41"),
        "only the first 40 lines are shown:\n{t}"
    );
    assert!(t.contains("… 10 more lines"), "{t}");

    // Short output is shown whole, without a truncation marker.
    let mut short = fresh();
    started(&mut short, "c1", "grep", serde_json::json!({}));
    finished(&mut short, "c1", "only line", false, 1);
    short.update(key(KeyCode::Tab));
    let t = text(&draw(&short, 80, 30));
    assert!(t.contains("only line") && !t.contains("more lines"), "{t}");
}

#[test]
fn a_running_expanded_card_says_so() {
    let mut app = fresh();
    started(
        &mut app,
        "c1",
        "run_shell",
        serde_json::json!({"command": "sleep 5"}),
    );
    app.update(key(KeyCode::Tab));
    let t = text(&draw(&app, 80, 24));
    assert!(t.contains("(running)"), "{t}");
}

#[test]
fn the_approval_modal_shows_the_request_a_coloured_diff_and_the_key_legend() {
    let mut app = fresh();
    let diff =
        "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,2 +1,2 @@\n context line\n-old line\n+new line\n";
    approval(
        &mut app,
        ApprovalRequest {
            id: 1,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "Overwrite hello.txt (9 bytes)".to_owned(),
            diff: Some(diff.to_owned()),
        },
    );
    settle(&mut app);
    let buffer = draw(&app, 100, 30);
    let t = text(&buffer);
    assert!(t.contains("Approval required"), "{t}");
    assert!(t.contains("Tool:    write_file"), "{t}");
    assert!(t.contains("Class:   write"), "{t}");
    assert!(t.contains("Overwrite hello.txt (9 bytes)"), "{t}");
    assert!(t.contains("[y] allow once"), "{t}");
    assert!(t.contains("[a] allow write for this session"), "{t}");
    assert!(t.contains("[n]/Esc deny"), "{t}");
    let (x, y) = find(&buffer, "+new line").expect("added line");
    let theme = &app.theme;
    assert_eq!(
        buffer[(x, y)].fg,
        theme.resolve(theme.success),
        "added lines are green"
    );
    assert_eq!(
        buffer[(x, y)].bg,
        theme.resolve(theme.diff_add_bg),
        "and tinted"
    );
    let (x, y) = find(&buffer, "-old line").expect("removed line");
    assert_eq!(
        buffer[(x, y)].fg,
        theme.resolve(theme.error),
        "removed lines are red"
    );
    assert_eq!(
        buffer[(x, y)].bg,
        theme.resolve(theme.diff_del_bg),
        "and tinted"
    );
    let (x, y) = find(&buffer, "@@ -1,2").expect("hunk header");
    assert_eq!(buffer[(x, y)].fg, theme.resolve(theme.accent));
    let (x, y) = find(&buffer, "+++ b/hello.txt").expect("file header");
    assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
    let (x, y) = find(&buffer, "context line").expect("context line");
    assert_ne!(buffer[(x, y)].fg, theme.resolve(theme.success));
    assert_ne!(buffer[(x, y)].fg, theme.resolve(theme.error));
}

#[test]
fn the_approval_modal_shows_a_shell_command_verbatim_without_a_diff() {
    let mut app = fresh();
    approval(
        &mut app,
        ApprovalRequest {
            id: 2,
            tool: "run_shell".to_owned(),
            class: ApprovalClass::Shell,
            summary: "Run: rm -rf build && cargo build --release".to_owned(),
            diff: None,
        },
    );
    let t = text(&draw(&app, 100, 24));
    assert!(t.contains("Class:   shell"), "{t}");
    assert!(
        t.contains("Run: rm -rf build && cargo build --release"),
        "{t}"
    );
    assert!(!t.contains("Diff:"), "{t}");
    assert!(t.contains("[a] allow shell for this session"), "{t}");
}

#[test]
fn a_huge_diff_is_cut_to_the_modal_and_the_legend_stays_visible() {
    let mut app = fresh();
    let diff: String = (0..500).map(|i| format!("+added line {i}\n")).collect();
    approval(
        &mut app,
        ApprovalRequest {
            id: 3,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "Create big.txt".to_owned(),
            diff: Some(diff),
        },
    );
    let t = text(&draw(&app, 80, 24));
    assert!(t.contains("more diff lines"), "{t}");
    assert!(
        t.contains("[y] allow once"),
        "the legend must never be pushed out:\n{t}"
    );
}

#[test]
fn the_drift_overlay_lists_changes_and_the_strategy_actions_before_a_canary_report() {
    let app = drift_app(vec![
        change("add", "description"),
        change("add", "input_schema.properties.a"),
    ]);
    let buffer = draw(&app, 110, 30);
    let t = text(&buffer);
    assert!(t.contains("Drift"), "{t}");
    assert!(t.contains("calc  2 change(s)  needs a decision"), "{t}");
    assert!(
        t.contains("add  description  [description, modified]"),
        "{t}"
    );
    assert!(
        t.contains("add  input_schema.properties.a  [description, modified]"),
        "{t}"
    );
    assert!(
        t.contains("[n] no update  [f] full rewrite  [s] selective patch"),
        "{t}"
    );
    assert!(
        !t.contains("[a] accept"),
        "no accept before a candidate exists:\n{t}"
    );
}

#[test]
fn the_drift_overlay_shows_progress_while_a_strategy_runs() {
    let mut app = drift_app(vec![change("add", "description")]);
    app.update(ch('f'));
    let t = text(&draw(&app, 110, 30));
    assert!(t.contains("Running full_rewrite and the canary"), "{t}");
    assert!(t.contains("calc  1 change(s)  running"), "{t}");
}

#[test]
fn the_drift_overlay_shows_pass_and_fail_per_probe_after_the_canary_report() {
    let mut app = drift_app(vec![change("add", "description")]);
    app.update(ch('f'));
    ui_event(
        &mut app,
        UiEvent::CanaryReady(canary(CanaryStatus::Pass, true)),
    );
    let buffer = draw(&app, 110, 30);
    let t = text(&buffer);
    assert!(t.contains("Strategy full_rewrite  canary pass"), "{t}");
    assert!(t.contains("✓ probe-add: returned 42"), "{t}");
    assert!(t.contains("✓ probe-shout control: returned hello"), "{t}");
    assert!(t.contains("Decision: promote"), "{t}");
    assert!(t.contains("[a] accept  [r] reject"), "{t}");
    let (x, y) = find(&buffer, "pass").expect("pass label");
    assert_ne!(buffer[(x, y)].fg, app.theme.resolve(app.theme.error));

    let mut failing = drift_app(vec![change("add", "description")]);
    failing.update(ch('f'));
    ui_event(
        &mut failing,
        UiEvent::CanaryReady(canary(CanaryStatus::Fail, false)),
    );
    settle(&mut failing);
    let buffer = draw(&failing, 110, 30);
    let t = text(&buffer);
    assert!(t.contains("canary fail"), "{t}");
    assert!(t.contains("✗ probe-shout control: returned hello"), "{t}");
    assert!(t.contains("A control probe failed"), "{t}");
    assert!(t.contains("Decision: blocked: control probe failed"), "{t}");
    assert!(
        t.contains("[r] reject  (this candidate cannot be accepted)"),
        "{t}"
    );
    assert!(
        !t.contains("[a] accept"),
        "a failed canary offers no accept:\n{t}"
    );
    let (x, y) = find(&buffer, "✗ probe-shout").expect("failed probe");
    assert_eq!(
        buffer[(x, y)].fg,
        failing.theme.resolve(failing.theme.error)
    );
}

#[test]
fn the_drift_overlay_says_so_when_there_is_no_drift() {
    let mut app = fresh();
    say(&mut app, "/drift");
    let t = text(&draw(&app, 100, 24));
    assert!(t.contains("No drift detected"), "{t}");
}

#[test]
fn help_picker_and_confirm_overlays_render() {
    let mut app = fresh();
    say(&mut app, "/help");
    let t = text(&draw(&app, 110, 40));
    for needle in [
        "Help",
        "/model [alias]",
        "/kb adopt <server>",
        "Ctrl-C",
        "y  allow once",
    ] {
        assert!(t.contains(needle), "{needle} missing:\n{t}");
    }
    app.update(key(KeyCode::Esc));

    say(&mut app, "/model");
    app.update(key(KeyCode::Down));
    settle(&mut app);
    let buffer = draw(&app, 110, 30);
    let t = text(&buffer);
    assert!(t.contains("Select a model"), "{t}");
    for alias in ["luna", "sol", "astra", "flash", "pro"] {
        assert!(t.contains(alias), "{alias} missing:\n{t}");
    }
    let (x, y) = find(&buffer, "› sol").expect("selected row");
    assert!(!buffer[(x, y)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(x, y)].bg, app.theme.resolve(app.theme.highlight));
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.accent));
    app.update(key(KeyCode::Esc));

    let mut app = App::new(app_init("luna", Effort::Max, &["docs"]));
    ui_event(
        &mut app,
        UiEvent::McpStatus(harness_tui::McpServerStatus {
            name: "docs".to_owned(),
            state: harness_tui::McpState::Connected,
            live_tools: 4,
            managed: false,
            exposed: vec![],
        }),
    );
    say(&mut app, "/kb adopt docs");
    let t = text(&draw(&app, 110, 30));
    assert!(t.contains("Adopt the 4 live tool(s) of docs"), "{t}");
    assert!(t.contains("[y] yes") && t.contains("[n]/Esc no"), "{t}");
}

#[test]
fn the_input_box_grows_up_to_six_lines_and_shows_the_cursor() {
    let mut app = fresh();
    type_text(&mut app, "abc");
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
    terminal.draw(|f| ui::render(f, &app)).expect("draw");
    let cursor = terminal.get_cursor_position().expect("cursor");
    // Border (1) + three typed characters; the input box sits above the status row.
    assert_eq!(
        (cursor.x, cursor.y),
        (6, 17),
        "single-line box is 3 rows tall: rows 16..=18; border (1) + prompt (2) + three characters"
    );

    for i in 0..9 {
        app.update(alt_enter());
        type_text(&mut app, &format!("row {i}"));
    }
    terminal.draw(|f| ui::render(f, &app)).expect("draw");
    let t = text(terminal.backend().buffer());
    // Six visible lines at most: the newest rows are shown, the oldest are scrolled out.
    assert!(t.contains("row 8") && t.contains("row 4"), "{t}");
    assert!(!t.contains("row 1\n") && !t.contains("row 2\n"), "{t}");
    let cursor = terminal.get_cursor_position().expect("cursor");
    assert!(
        cursor.y <= 18,
        "the cursor stays inside the box: {cursor:?}"
    );

    // An overlay hides the cursor target by not setting it; the app stays drawable.
    say(&mut app, "/help");
    terminal
        .draw(|f| ui::render(f, &app))
        .expect("draw with overlay");
}

#[test]
fn scrolling_shows_older_lines_and_following_shows_the_newest() {
    let mut app = fresh();
    for i in 0..60 {
        ui_event(&mut app, UiEvent::Notice(format!("note {i:02}")));
    }
    let bottom = text(&draw(&app, 60, 12));
    assert!(bottom.contains("note 59"), "{bottom}");
    assert!(!bottom.contains("note 00"), "{bottom}");
    for _ in 0..6 {
        app.update(key(KeyCode::PageUp));
    }
    settle(&mut app);
    let top = text(&draw(&app, 60, 12));
    assert!(top.contains("note 00"), "{top}");
    assert!(!top.contains("note 59"), "{top}");
    // Scrolling past the start clamps instead of panicking.
    for _ in 0..50 {
        app.update(key(KeyCode::PageUp));
    }
    settle(&mut app);
    let clamped = text(&draw(&app, 60, 12));
    assert!(clamped.contains("note 00"), "{clamped}");
}

#[test]
fn an_eighty_by_twenty_four_terminal_survives_long_lines_everywhere() {
    let long = "supercalifragilisticexpialidocious".repeat(20);
    let mut app = fresh();
    say(&mut app, &long);
    agent_event(
        &mut app,
        AgentUiEvent::TextDelta(format!("{long} and {}", "word ".repeat(200))),
    );
    started(&mut app, "c1", &long, serde_json::json!({"a": long}));
    finished(&mut app, "c1", &format!("{long}\n{long}"), true, 7);
    app.update(key(KeyCode::Tab));
    ui_event(&mut app, UiEvent::Error(long.clone()));
    ui_event(&mut app, UiEvent::Notice(long.clone()));
    type_text(&mut app, &long);
    for _ in 0..8 {
        app.update(alt_enter());
        type_text(&mut app, &long);
    }
    draw(&app, 80, 24);

    // Every overlay, with long content, at 80x24.
    approval(
        &mut app,
        ApprovalRequest {
            id: 9,
            tool: long.clone(),
            class: ApprovalClass::Shell,
            summary: long.clone(),
            diff: Some(format!("+{long}\n-{long}\n@@ {long}")),
        },
    );
    draw(&app, 80, 24);

    let mut drift = drift_app(
        (0..40)
            .map(|i| change(&format!("tool{i}{long}"), "description"))
            .collect(),
    );
    draw(&drift, 80, 24);
    drift.update(ch('f'));
    ui_event(
        &mut drift,
        UiEvent::CanaryReady(canary(CanaryStatus::Fail, false)),
    );
    draw(&drift, 80, 24);

    let mut other = fresh();
    say(&mut other, "/help");
    draw(&other, 80, 24);
    other.update(key(KeyCode::Esc));
    say(&mut other, "/model");
    draw(&other, 80, 24);
}

#[test]
fn tiny_terminals_do_not_panic() {
    let mut app = fresh();
    say(&mut app, "hello");
    agent_event(&mut app, AgentUiEvent::TextDelta("streaming".to_owned()));
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "a"}),
    );
    app.update(key(KeyCode::Tab));
    for (w, h) in [(1, 1), (2, 2), (10, 3), (20, 5), (30, 8), (5, 40)] {
        draw(&app, w, h);
    }
    approval(
        &mut app,
        ApprovalRequest {
            id: 1,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "s".to_owned(),
            diff: Some("+a\n-b\n".to_owned()),
        },
    );
    for (w, h) in [(1, 1), (6, 4), (20, 6), (40, 10)] {
        draw(&app, w, h);
    }
    let mut drift = drift_app(vec![change("add", "description")]);
    for (w, h) in [(1, 1), (6, 4), (20, 6), (40, 10)] {
        draw(&drift, w, h);
    }
    drift.update(key(KeyCode::Esc));
    say(&mut drift, "/help");
    for (w, h) in [(1, 1), (6, 4), (20, 6)] {
        draw(&drift, w, h);
    }
}

// -------------------------------------------------------------------------------------------
// Theme, popups and colour support
// -------------------------------------------------------------------------------------------

fn any_reversed(buffer: &ratatui::buffer::Buffer) -> bool {
    buffer
        .content()
        .iter()
        .any(|cell| cell.modifier.contains(Modifier::REVERSED))
}

fn distinct_colours(buffer: &ratatui::buffer::Buffer) -> usize {
    let mut set = BTreeSet::new();
    for cell in buffer.content() {
        set.insert(format!("{:?}", cell.fg));
        set.insert(format!("{:?}", cell.bg));
    }
    set.len()
}

#[test]
fn the_whole_frame_is_painted_with_the_theme_background() {
    let app = fresh();
    let buffer = draw(&app, 80, 24);
    let bg = app.theme.resolve(app.theme.bg);
    assert_eq!(buffer[(0, 0)].bg, bg);
    assert_eq!(buffer[(79, 5)].bg, bg);
    assert_eq!(buffer[(0, 0)].fg, app.theme.resolve(app.theme.text));
    // The status bar sits on the raised surface.
    assert_eq!(buffer[(79, 23)].bg, app.theme.resolve(app.theme.raised));
}

#[test]
fn user_messages_have_a_coloured_bar_and_assistant_text_a_gutter() {
    let mut app = fresh();
    say(&mut app, "hello agent");
    agent_event(&mut app, AgentUiEvent::TextDelta("hi there".to_owned()));
    settle(&mut app);
    let buffer = draw(&app, 80, 24);
    let (x, y) = find(&buffer, "▎").expect("user bar");
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.user));
    let (x, y) = find(&buffer, "hello agent").expect("user text");
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.user));
    let (x, y) = find(&buffer, "hi there").expect("assistant text");
    assert_eq!(buffer[(x, y)].fg, app.theme.resolve(app.theme.text));
    assert!(find(&buffer, "▏").is_some(), "assistant gutter");
}

#[test]
fn tool_cards_sit_on_the_raised_surface_with_a_rail_and_chips() {
    let mut app = fresh();
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "a"}),
    );
    finished(&mut app, "c1", "x", false, 9);
    started(&mut app, "c2", "grep", serde_json::json!({"p": 1}));
    app.update(key(KeyCode::Tab));
    settle(&mut app);
    let buffer = draw(&app, 80, 30);
    let theme = &app.theme;
    let (x, y) = find(&buffer, "read_file").expect("card name");
    assert_eq!(buffer[(x, y)].fg, theme.resolve(theme.accent));
    assert_eq!(buffer[(x, y)].bg, theme.resolve(theme.raised));
    // The card background reaches the right edge of the transcript.
    assert_eq!(buffer[(78, y)].bg, theme.resolve(theme.raised));
    // The focused (expanded) card is highlighted.
    let (x, y) = find(&buffer, "(running)").expect("expanded running card");
    assert_eq!(buffer[(x, y)].bg, theme.resolve(theme.highlight));
    assert!(find(&buffer, "╭").is_some(), "rail top");
    assert!(find(&buffer, "│").is_some(), "rail body");
    assert!(find(&buffer, "╰").is_some(), "rail end");
}

#[test]
fn diff_like_tool_output_is_tinted() {
    let mut app = fresh();
    started(&mut app, "c1", "edit_file", serde_json::json!({}));
    finished(
        &mut app,
        "c1",
        "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n",
        false,
        4,
    );
    app.update(key(KeyCode::Tab));
    let buffer = draw(&app, 80, 30);
    let theme = &app.theme;
    let (x, y) = find(&buffer, "+new").expect("added");
    assert_eq!(buffer[(x, y)].bg, theme.resolve(theme.diff_add_bg));
    let (x, y) = find(&buffer, "-old").expect("removed");
    assert_eq!(buffer[(x, y)].bg, theme.resolve(theme.diff_del_bg));
}

#[test]
fn popups_are_rounded_padded_and_leave_the_text_below_dimmed() {
    let mut app = fresh();
    for i in 0..30 {
        ui_event(
            &mut app,
            UiEvent::Notice(format!("note {i:02} keeps the transcript busy")),
        );
    }
    settle(&mut app);
    let before = draw(&app, 120, 40);
    let (nx, ny) = find(&before, "note 29").expect("notice before");
    let undimmed = before[(nx, ny)].fg;

    say(&mut app, "/model");
    settle(&mut app);
    let after = draw(&app, 120, 40);
    let theme = &app.theme;
    // Rounded border on the raised surface.
    let (cx, cy) = find(&after, "╭").expect("rounded popup border");
    assert_eq!(after[(cx, cy)].bg, theme.resolve(theme.raised));
    // Padding: the cells just inside the border are blank, on every side.
    assert_eq!(after[(cx + 1, cy + 1)].symbol(), " ", "top padding row");
    assert_eq!(after[(cx + 1, cy + 2)].symbol(), " ", "left padding column");
    let (sx, sy) = find(&after, "› luna").expect("first row");
    assert_eq!(
        sx,
        cx + 2,
        "content starts after the border and one padding column"
    );
    assert_eq!(
        sy,
        cy + 2,
        "content starts after the border and one padding row"
    );
    // The transcript outside the popup is dimmed toward the background.
    let (nx2, ny2) = find(&after, "note 29").expect("notice stays visible");
    let dimmed = after[(nx2, ny2)].fg;
    assert_ne!(dimmed, undimmed, "text under a popup is dimmed");
    let bg = theme.resolve(theme.bg);
    let (Color::Rgb(a, b, c), Color::Rgb(d, e, f), Color::Rgb(g, h, i)) = (undimmed, dimmed, bg)
    else {
        panic!("true colour expected");
    };
    let dist = |x: u8, y: u8| i32::from(x).abs_diff(i32::from(y));
    assert!(
        dist(d, g) + dist(e, h) + dist(f, i) < dist(a, g) + dist(b, h) + dist(c, i),
        "dimmed text is closer to the background"
    );
}

#[test]
fn popups_fade_in_over_the_open_animation_and_end_fully_opaque() {
    let mut app = fresh();
    say(&mut app, "/model");
    let first = draw(&app, 100, 30);
    let (x, y) = find(&first, "Select a model").expect("title");
    let start = first[(x, y)].fg;
    let mut last = start;
    let mut changed = false;
    for _ in 0..10 {
        app.update(Msg::Tick);
        let next = draw(&app, 100, 30)[(x, y)].fg;
        changed |= next != last;
        last = next;
    }
    assert!(changed, "the title colour changes while the popup opens");
    assert_ne!(start, last);
    assert_eq!(
        last,
        app.theme.resolve(app.theme.accent),
        "fully opaque at the end"
    );
    // Settled frames are stable.
    app.update(Msg::Tick);
    assert_eq!(draw(&app, 100, 30)[(x, y)].fg, last);
}

#[test]
fn no_rendered_frame_uses_reverse_video() {
    let mut app = fresh();
    say(&mut app, "hello");
    agent_event(&mut app, AgentUiEvent::TextDelta("streaming".to_owned()));
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "a"}),
    );
    app.update(key(KeyCode::Tab));
    settle(&mut app);
    assert!(!any_reversed(&draw(&app, 100, 30)));
    approval(
        &mut app,
        ApprovalRequest {
            id: 1,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "s".to_owned(),
            diff: Some("+a\n-b\n".to_owned()),
        },
    );
    settle(&mut app);
    assert!(!any_reversed(&draw(&app, 100, 30)));
    let drift = drift_app(vec![change("add", "description")]);
    assert!(!any_reversed(&draw(&drift, 110, 30)));
    for no_color in [false, true] {
        let mut other = fresh();
        other.theme.no_color = no_color;
        say(&mut other, "/help");
        assert!(!any_reversed(&draw(&other, 100, 40)));
        other.update(key(KeyCode::Esc));
        say(&mut other, "/model");
        settle(&mut other);
        assert!(!any_reversed(&draw(&other, 100, 40)));
        other.update(key(KeyCode::Esc));
        assert!(!any_reversed(&draw(&other, 100, 40)));
    }
}

#[test]
fn no_color_uses_terminal_defaults_and_modifiers_only() {
    let mut app = fresh();
    app.theme.no_color = true;
    say(&mut app, "/model");
    settle(&mut app);
    let buffer = draw(&app, 100, 30);
    for cell in buffer.content() {
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
    let (x, y) = find(&buffer, "› luna").expect("selected row");
    assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
    assert!(buffer[(x, y)].modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn colour_support_levels_change_the_cell_colours_but_not_the_text() {
    use harness_tui::theme::ColorSupport;
    let mut app = fresh();
    say(&mut app, "hello");
    let truecolor = text(&draw(&app, 80, 24));
    app.theme.support = ColorSupport::Indexed256;
    let indexed = draw(&app, 80, 24);
    assert_eq!(text(&indexed), truecolor);
    assert!(
        indexed
            .content()
            .iter()
            .all(|c| !matches!(c.fg, Color::Rgb(..)) && !matches!(c.bg, Color::Rgb(..)))
    );
    app.theme.support = ColorSupport::Basic;
    let basic = draw(&app, 80, 24);
    assert!(
        basic
            .content()
            .iter()
            .all(|c| !matches!(c.fg, Color::Rgb(..) | Color::Indexed(_))
                && !matches!(c.bg, Color::Rgb(..) | Color::Indexed(_)))
    );
    assert!(distinct_colours(&basic) >= 3);
    // A popup over a screen with fewer colours still renders.
    say(&mut app, "/help");
    draw(&app, 80, 24);
}

#[test]
fn the_help_overlay_scrolls_on_a_short_terminal_and_says_there_is_more() {
    let mut app = fresh();
    app.viewport = (100, 16);
    say(&mut app, "/help");
    settle(&mut app);
    let top = text(&draw(&app, 100, 16));
    assert!(top.contains("Keys"), "{top}");
    assert!(
        !top.contains("/quit"),
        "the last command is clipped:\n{top}"
    );
    assert!(top.contains("more"), "a clipped help says so:\n{top}");
    for _ in 0..40 {
        app.update(key(KeyCode::Down));
    }
    let bottom = text(&draw(&app, 100, 16));
    assert!(
        bottom.contains("/quit"),
        "the end of help is reachable:\n{bottom}"
    );
    assert!(
        !bottom.contains("Keys"),
        "the top has scrolled away:\n{bottom}"
    );
    for _ in 0..40 {
        app.update(key(KeyCode::Up));
    }
    assert!(text(&draw(&app, 100, 16)).contains("Keys"));
    app.update(key(KeyCode::PageDown));
    assert_ne!(text(&draw(&app, 100, 16)), top);
    app.update(key(KeyCode::End));
    assert!(text(&draw(&app, 100, 16)).contains("/quit"));
    app.update(key(KeyCode::Home));
    assert!(text(&draw(&app, 100, 16)).contains("Keys"));
    assert!(app.overlay.is_some(), "scroll keys do not close the help");
    app.update(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
}

#[test]
fn the_cost_figure_warms_as_the_budget_is_spent() {
    let mut app = fresh();
    ui_event(
        &mut app,
        UiEvent::Cost(CostSnapshot {
            spent_usd: 4.8,
            unreconciled_usd: 0.0,
            remaining_usd: 0.2,
            cap_usd: 5.0,
        }),
    );
    settle(&mut app);
    let buffer = draw(&app, 100, 24);
    let (x, y) = find(&buffer, "$4.8000").expect("spend");
    assert_ne!(
        buffer[(x, y)].fg,
        app.theme.resolve(app.theme.text),
        "a nearly spent budget is not drawn in the calm colour"
    );
}

#[test]
fn every_screen_renders_at_eighty_by_twenty_four_and_in_tiny_terminals() {
    let mut app = fresh();
    say(&mut app, "hello");
    agent_event(&mut app, AgentUiEvent::TextDelta("x".repeat(500)));
    started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "a"}),
    );
    app.update(key(KeyCode::Tab));
    for (w, h) in [(80, 24), (1, 1), (3, 3), (7, 5), (12, 6), (9, 30)] {
        draw(&app, w, h);
    }
    for overlay in ["/help", "/model", "/drift"] {
        let mut other = fresh();
        say(&mut other, overlay);
        for (w, h) in [(80, 24), (1, 1), (3, 3), (7, 5), (12, 6), (9, 30), (30, 7)] {
            draw(&other, w, h);
        }
    }
}

// -------------------------------------------------------------------------------------------
// Overlays stay inside the chat region
// -------------------------------------------------------------------------------------------

fn overlay_apps() -> Vec<(&'static str, App)> {
    let mut help = fresh();
    say(&mut help, "/help");
    let mut picker = fresh();
    say(&mut picker, "/model");
    let mut confirm = App::new(app_init("luna", Effort::Max, &["docs"]));
    ui_event(
        &mut confirm,
        UiEvent::McpStatus(harness_tui::McpServerStatus {
            name: "docs".to_owned(),
            state: harness_tui::McpState::Connected,
            live_tools: 4,
            managed: false,
            exposed: vec![],
        }),
    );
    say(&mut confirm, "/kb adopt docs");
    let mut approval_app = fresh();
    approval(
        &mut approval_app,
        ApprovalRequest {
            id: 1,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "Create big.txt".to_owned(),
            diff: Some((0..60).map(|i| format!("+added line {i}\n")).collect()),
        },
    );
    let mut drift = drift_app(
        (0..30)
            .map(|i| change(&format!("tool{i}"), "description"))
            .collect(),
    );
    drift.update(ch('f'));
    ui_event(
        &mut drift,
        UiEvent::CanaryReady(canary(CanaryStatus::Pass, true)),
    );
    vec![
        ("help", help),
        ("approval", approval_app),
        ("picker", picker),
        ("confirm", confirm),
        ("drift", drift),
    ]
}

#[test]
fn overlays_never_touch_the_input_box_or_the_status_bar() {
    for (w, h) in [(100, 30), (80, 24), (60, 12)] {
        for (name, mut app) in overlay_apps() {
            settle(&mut app);
            assert!(app.overlay.is_some(), "{name} is open");
            let with = draw(&app, w, h);
            let mut closed = app.clone();
            closed.overlay = None;
            let without = draw(&closed, w, h);
            // Single-line input: border (3 rows) plus the status row.
            for y in (h - 4)..h {
                for x in 0..w {
                    assert_eq!(
                        with[(x, y)],
                        without[(x, y)],
                        "{name} at {w}x{h}: cell ({x},{y}) differs from the no-overlay frame"
                    );
                }
            }
            let t = text(&with);
            assert!(
                t.lines().last().expect("status").contains("luna/max"),
                "{name}"
            );
        }
    }
}

#[test]
fn the_approval_footer_survives_a_long_diff_at_eighty_by_twenty_four() {
    for (name, mut app) in overlay_apps() {
        if name != "approval" {
            continue;
        }
        settle(&mut app);
        let t = text(&draw(&app, 80, 24));
        assert!(t.contains("[y] allow once"), "{t}");
        assert!(t.contains("[n]/Esc deny"), "{t}");
        assert!(t.contains("more diff lines"), "{t}");
    }
}

#[test]
fn the_drift_body_says_when_it_is_cut() {
    for (name, mut app) in overlay_apps() {
        if name != "drift" {
            continue;
        }
        settle(&mut app);
        let t = text(&draw(&app, 80, 24));
        assert!(t.contains("more lines") || t.contains("more change"), "{t}");
        assert!(t.contains("[a] accept"), "{t}");
    }
}

#[test]
fn overlays_survive_chat_regions_smaller_than_any_popup() {
    for (_, mut app) in overlay_apps() {
        settle(&mut app);
        for (w, h) in [(80, 5), (80, 6), (40, 7), (10, 5), (3, 3), (1, 1), (100, 8)] {
            draw(&app, w, h);
        }
    }
}

#[test]
fn a_named_theme_paints_its_background_on_the_frame() {
    use harness_tui::theme::config::ThemeSpec;
    let mut app = fresh();
    let spec = ThemeSpec::default()
        .with_builtin("gruvbox-dark-hard")
        .expect("built-in exists");
    app.set_theme_spec(spec);
    let buffer = draw(&app, 100, 30);
    assert_eq!(buffer[(0, 0)].bg, Color::Rgb(0x1d, 0x20, 0x21));
}
