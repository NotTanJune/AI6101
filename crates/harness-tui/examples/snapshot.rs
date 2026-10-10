//! Prints the main screens as plain text, rendered by the real renderer into a `TestBackend`.
//!
//! A review aid: it shows layout, padding and overlap without a terminal. Each screen is drawn
//! at 100x30 after the animations have settled, followed by the number of distinct colours it
//! uses. Run with `cargo run -p harness-tui --example snapshot`.
//!
//! An optional third argument picks a built-in colour theme, for example
//! `snapshot 100 30 gruvbox-dark-hard`; each screen header names the theme.

use std::collections::BTreeSet;
use std::path::Path;

use harness_agent::approval::{ApprovalClass, ApprovalRequest};
use harness_agent::drift::DriftReport;
use harness_agent::model::TokenUsage;
use harness_agent::registry::{Effort, ModelRegistry};
use harness_core::diff::KbDiff;
use harness_core::{CanaryStatus, ChangeClass, ChangeKind, ChangeRecord, Strategy};
use harness_tui::theme::builtin;
use harness_tui::theme::config::ThemeSpec;
use harness_tui::{
    AgentUiEvent, App, AppInit, CanarySummary, CostSnapshot, McpServerStatus, McpState, ModelInfo,
    Msg, ProbeLine, UiEvent, ui,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Terminal size: `snapshot [width height]`, default 100x30.
fn size() -> (u16, u16) {
    let args: Vec<u16> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    match args.as_slice() {
        [w, h, ..] => (*w, *h),
        _ => (100, 30),
    }
}

/// The theme named by the optional third argument (default `tokyo-night`). An unknown name
/// exits with status 2 and lists the slugs.
fn chosen_theme() -> ThemeSpec {
    let Some(name) = std::env::args().nth(3) else {
        return ThemeSpec::default();
    };
    match builtin::find(&name) {
        Some(found) => ThemeSpec::default()
            .with_builtin(found.slug)
            .unwrap_or_default(),
        None => {
            eprintln!(
                "unknown theme {name:?}; valid themes: {}",
                builtin::names().join(", ")
            );
            std::process::exit(2);
        }
    }
}

fn new_app() -> App {
    let registry = ModelRegistry::load(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/models.toml"),
    )
    .expect("configs/models.toml loads");
    let mut app = App::new(AppInit {
        models: registry.models().iter().map(ModelInfo::from_spec).collect(),
        model_alias: "luna".to_owned(),
        effort: Effort::Max,
        budget_usd: 5.0,
        workspace: "/Users/dev/projects/acme-api".to_owned(),
        mcp_servers: vec!["calc".to_owned(), "docs".to_owned()],
    });
    app.set_theme_spec(chosen_theme());
    app
}

fn settle(app: &mut App) {
    for _ in 0..120 {
        app.update(Msg::Tick);
    }
}

fn key(app: &mut App, code: KeyCode) {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn agent(app: &mut App, event: AgentUiEvent) {
    app.update(Msg::Ui(UiEvent::Agent(event)));
}

fn draw(app: &App) -> Buffer {
    let (width, height) = size();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| ui::render(frame, app)).expect("draw");
    terminal.backend().buffer().clone()
}

fn dump(title: &str, app: &App) {
    let (width, height) = size();
    let buffer = draw(app);
    let mut colours = BTreeSet::new();
    for cell in buffer.content() {
        colours.insert(format!("fg {:?}", cell.fg));
        colours.insert(format!("bg {:?}", cell.bg));
    }
    let fg = colours.iter().filter(|c| c.starts_with("fg")).count();
    let bg = colours.iter().filter(|c| c.starts_with("bg")).count();
    println!(
        "=== {title} ({width}x{height}, theme {}) ===",
        app.theme_spec.label()
    );
    println!("+{}+", "-".repeat(usize::from(width)));
    for y in 0..height {
        let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
        println!("|{row}|");
    }
    println!("+{}+", "-".repeat(usize::from(width)));
    println!("distinct colours: {fg} foreground, {bg} background\n");
}

fn tool_started(app: &mut App, id: &str, name: &str, args: serde_json::Value) {
    agent(
        app,
        AgentUiEvent::ToolCallStarted {
            call_id: id.to_owned(),
            name: name.to_owned(),
            arguments: args,
        },
    );
}

fn tool_finished(app: &mut App, id: &str, output: &str, is_error: bool, ms: u64) {
    agent(
        app,
        AgentUiEvent::ToolCallFinished {
            call_id: id.to_owned(),
            output: output.to_owned(),
            is_error,
            duration_ms: ms,
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

fn drift_report() -> DriftReport {
    let changes = vec![
        change("add", "description"),
        change("add", "input_schema.properties.a"),
        change("shout", "description"),
    ];
    DriftReport {
        server: "calc".to_owned(),
        kb_cards: 3,
        live_tools: 4,
        diff: KbDiff {
            source_version: "kb-1".to_owned(),
            target_version: "live".to_owned(),
            affected_lineage_ids: changes.iter().map(|c| c.lineage_id.clone()).collect(),
            changes,
        },
    }
}

fn canary() -> CanarySummary {
    CanarySummary {
        server: "calc".to_owned(),
        strategy: Strategy::FullRewrite,
        status: CanaryStatus::Pass,
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
                status: CanaryStatus::Pass,
                expected: Some("HELLO".to_owned()),
                actual: Some("returned HELLO".to_owned()),
            },
        ],
        collateral_damage: false,
        decision: "promote".to_owned(),
        can_accept: true,
    }
}

fn conversation() -> App {
    let mut app = new_app();
    app.update(Msg::Ui(UiEvent::McpStatus(McpServerStatus {
        name: "calc".to_owned(),
        state: McpState::Connected,
        live_tools: 4,
        managed: true,
        exposed: vec![],
    })));
    type_text(&mut app, "Fix the typo in README.md and run the tests");
    key(&mut app, KeyCode::Enter);
    agent(
        &mut app,
        AgentUiEvent::TextDelta("I'll look at the README first.".to_owned()),
    );
    tool_started(
        &mut app,
        "c1",
        "read_file",
        serde_json::json!({"path": "README.md"}),
    );
    tool_finished(&mut app, "c1", "# Acme API\n\nA smal service.\n", false, 12);
    tool_started(
        &mut app,
        "c2",
        "edit_file",
        serde_json::json!({"path": "README.md", "old": "smal", "new": "small"}),
    );
    tool_finished(
        &mut app,
        "c2",
        "--- a/README.md\n+++ b/README.md\n@@ -2,3 +2,3 @@\n # Acme API\n \n-A smal service.\n+A small service.\n",
        false,
        31,
    );
    tool_started(
        &mut app,
        "c3",
        "run_shell",
        serde_json::json!({"command": "cargo test"}),
    );
    tool_finished(&mut app, "c3", "exit status 101", true, 8040);
    agent(
        &mut app,
        AgentUiEvent::Usage {
            model_alias: "luna".to_owned(),
            usage: TokenUsage {
                input: Some(12_400),
                output_including_reasoning: Some(840),
                ..TokenUsage::default()
            },
            cost_usd: Some(0.0412),
        },
    );
    app.update(Msg::Ui(UiEvent::Cost(CostSnapshot {
        spent_usd: 0.0412,
        unreconciled_usd: 0.0,
        remaining_usd: 4.9588,
        cap_usd: 5.0,
    })));
    // Expand the first tool card (the focused card is the last one, so cycle focus back).
    key(&mut app, KeyCode::BackTab);
    key(&mut app, KeyCode::BackTab);
    key(&mut app, KeyCode::Tab);
    agent(
        &mut app,
        AgentUiEvent::TextDelta(
            "The typo is fixed, but the tests fail on a missing fixture".to_owned(),
        ),
    );
    app
}

/// The same conversation after the turn ended, so overlays that need an idle agent open.
fn idle_conversation() -> App {
    let mut app = conversation();
    agent(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: harness_agent::events::StopReason::Completed,
        },
    );
    app
}

fn main() {
    // Validate the theme argument before drawing anything.
    chosen_theme();
    let mut app = new_app();
    settle(&mut app);
    dump("empty welcome", &app);

    let mut app = conversation();
    settle(&mut app);
    dump(
        "conversation: streamed reply, collapsed and expanded tool cards",
        &app,
    );

    let mut app = conversation();
    app.update(Msg::Ui(UiEvent::ApprovalNeeded {
        id: 1,
        request: ApprovalRequest {
            id: 1,
            tool: "write_file".to_owned(),
            class: ApprovalClass::Write,
            summary: "Overwrite README.md (28 bytes)".to_owned(),
            diff: Some(
                "--- a/README.md\n+++ b/README.md\n@@ -1,3 +1,3 @@\n # Acme API\n \n-A smal service.\n+A small service.\n"
                    .to_owned(),
            ),
        },
    }));
    settle(&mut app);
    dump("approval prompt with diff", &app);

    let mut app = idle_conversation();
    type_text(&mut app, "/help");
    key(&mut app, KeyCode::Enter);
    settle(&mut app);
    dump("help overlay", &app);

    let mut app = idle_conversation();
    type_text(&mut app, "/mo");
    settle(&mut app);
    dump("/mo autocomplete popup", &app);

    let mut app = idle_conversation();
    type_text(&mut app, "/model");
    key(&mut app, KeyCode::Enter);
    settle(&mut app);
    // The input holds `/model`; suggestions are hidden while an overlay is open.
    dump("model picker", &app);

    let mut app = idle_conversation();
    app.update(Msg::Ui(UiEvent::DriftReady {
        server: "calc".to_owned(),
        report: drift_report(),
    }));
    type_text(&mut app, "/drift");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('f'));
    app.update(Msg::Ui(UiEvent::CanaryReady(canary())));
    settle(&mut app);
    dump("drift review (candidate ready)", &app);
}
