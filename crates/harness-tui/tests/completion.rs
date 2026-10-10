//! Slash command autocomplete: reducer behaviour and rendering.

mod common;

use common::*;
use harness_agent::registry::Effort;
use harness_tui::completion::MAX_ROWS;
use harness_tui::{App, Entry, McpServerStatus, McpState, Msg, Overlay, UiCommand, UiEvent};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn inserts(app: &App) -> Vec<String> {
    app.suggestions()
        .map(|c| c.items.iter().map(|i| i.insert.clone()).collect())
        .unwrap_or_default()
}

fn app_with_servers() -> App {
    let mut app = App::new(app_init("luna", Effort::Max, &["calc", "docs"]));
    ui_event(
        &mut app,
        UiEvent::McpStatus(McpServerStatus {
            name: "calc".to_owned(),
            state: McpState::Connected,
            live_tools: 3,
            managed: false,
            exposed: vec![],
        }),
    );
    app
}

// -------------------------------------------------------------------------------------------
// Reducer
// -------------------------------------------------------------------------------------------

#[test]
fn typing_a_slash_lists_the_commands_and_narrows_as_you_type() {
    let mut app = fresh();
    assert!(app.suggestions().is_none(), "nothing before a slash");
    type_text(&mut app, "/");
    assert_eq!(inserts(&app).len(), 10);
    type_text(&mut app, "m");
    assert_eq!(inserts(&app), ["/model", "/mcp"]);
    type_text(&mut app, "o");
    assert_eq!(inserts(&app), ["/model"]);
    app.update(key(KeyCode::Backspace));
    assert_eq!(inserts(&app), ["/model", "/mcp"]);
    app.update(key(KeyCode::Backspace));
    app.update(key(KeyCode::Backspace));
    assert!(app.suggestions().is_none(), "an empty input has none");
}

#[test]
fn tab_accepts_the_selected_suggestion_and_adds_a_space_when_arguments_follow() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    let effects = app.update(key(KeyCode::Tab));
    assert!(effects.is_empty());
    assert_eq!(app.input.text(), "/model ");
    assert_eq!(
        inserts(&app),
        ["luna", "sol", "astra", "flash", "pro"],
        "model aliases come next"
    );
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Tab));
    assert_eq!(
        app.input.text(),
        "/model sol",
        "the selection was moved to sol"
    );
    assert!(
        app.suggestions().is_none(),
        "a complete alias has nothing to add"
    );

    let mut other = fresh();
    type_text(&mut other, "/he");
    other.update(key(KeyCode::Tab));
    assert_eq!(
        other.input.text(),
        "/help",
        "no trailing space for a command without arguments"
    );
}

#[test]
fn up_and_down_move_the_selection_and_clamp_at_the_ends() {
    let mut app = fresh();
    type_text(&mut app, "/m");
    assert_eq!(app.suggest.selected, 0);
    app.update(key(KeyCode::Up));
    assert_eq!(app.suggest.selected, 0);
    app.update(key(KeyCode::Down));
    assert_eq!(app.suggest.selected, 1);
    app.update(key(KeyCode::Down));
    assert_eq!(app.suggest.selected, 1, "clamped at the last row");
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/mcp ");
    // Typing resets the selection.
    let mut again = fresh();
    type_text(&mut again, "/m");
    again.update(key(KeyCode::Down));
    type_text(&mut again, "c");
    assert_eq!(again.suggest.selected, 0);
}

#[test]
fn enter_accepts_when_the_command_is_incomplete_and_submits_when_it_is_complete() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    let effects = app.update(key(KeyCode::Enter));
    assert!(effects.is_empty(), "accepting sends nothing");
    assert_eq!(app.input.text(), "/model ");
    assert!(app.overlay.is_none());
    assert!(app.transcript.is_empty(), "no error entry was produced");

    // `/model ` is a complete command: Enter now opens the picker.
    app.update(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Picker(_))));
    assert!(app.input.is_empty());

    // An unknown alias is not complete: Enter completes it.
    let mut partial = fresh();
    type_text(&mut partial, "/model fl");
    partial.update(key(KeyCode::Enter));
    assert_eq!(partial.input.text(), "/model flash");
    let effects = partial.update(key(KeyCode::Enter));
    assert!(
        matches!(
            sent(&effects).as_slice(),
            [UiCommand::SetModel { alias, .. }] if alias == "flash"
        ),
        "{effects:?}"
    );
}

#[test]
fn escape_dismisses_the_suggestions_until_the_text_changes() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    assert!(app.suggestions().is_some());
    app.update(key(KeyCode::Esc));
    assert!(app.suggestions().is_none());
    assert_eq!(app.input.text(), "/mo", "Esc keeps the text");
    // While dismissed, Enter submits normally (an unknown command here).
    app.update(key(KeyCode::Enter));
    assert!(
        matches!(app.transcript.last(), Some(Entry::Error(e)) if e.contains("unknown command /mo")),
        "{:?}",
        app.transcript
    );

    let mut typed = fresh();
    type_text(&mut typed, "/mo");
    typed.update(key(KeyCode::Esc));
    type_text(&mut typed, "d");
    assert!(
        typed.suggestions().is_some(),
        "a changed text brings them back"
    );
    typed.update(key(KeyCode::Esc));
    typed.update(key(KeyCode::Backspace));
    assert!(typed.suggestions().is_some());
}

#[test]
fn paths_and_normal_messages_never_trigger_suggestions() {
    let mut app = fresh();
    type_text(&mut app, "/etc/hosts is broken");
    assert!(app.suggestions().is_none());
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(
        sent(&effects).as_slice(),
        [UiCommand::SendUser(text)] if text == "/etc/hosts is broken"
    ));
    let mut word = fresh();
    type_text(&mut word, "/etc");
    assert!(
        word.suggestions().is_none(),
        "a non-command word after the slash"
    );
    let mut plain = fresh();
    type_text(&mut plain, "hello /mo");
    assert!(plain.suggestions().is_none());
}

#[test]
fn effort_levels_come_from_the_current_model() {
    let mut luna = fresh();
    type_text(&mut luna, "/effort ");
    assert_eq!(
        inserts(&luna),
        ["none", "low", "medium", "high", "xhigh", "max"]
    );
    let mut flash = App::new(app_init("flash", Effort::High, &[]));
    type_text(&mut flash, "/effort ");
    assert_eq!(
        inserts(&flash),
        ["low", "medium", "high"],
        "flash has no xhigh"
    );
    type_text(&mut flash, "h");
    flash.update(key(KeyCode::Tab));
    assert_eq!(flash.input.text(), "/effort high");
}

#[test]
fn mcp_and_kb_complete_subcommands_then_server_names() {
    let mut app = app_with_servers();
    type_text(&mut app, "/mcp ");
    assert_eq!(inserts(&app), ["connect", "disconnect"]);
    type_text(&mut app, "c");
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/mcp connect ");
    assert_eq!(inserts(&app), ["calc", "docs"]);
    type_text(&mut app, "d");
    app.update(key(KeyCode::Enter));
    assert_eq!(app.input.text(), "/mcp connect docs");
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(
        sent(&effects).as_slice(),
        [UiCommand::ConnectMcp(name)] if name == "docs"
    ));

    let mut kb = app_with_servers();
    type_text(&mut kb, "/kb ");
    assert_eq!(inserts(&kb), ["adopt"]);
    kb.update(key(KeyCode::Tab));
    assert_eq!(kb.input.text(), "/kb adopt ");
    assert_eq!(inserts(&kb), ["calc", "docs"]);
    kb.update(key(KeyCode::Tab));
    assert_eq!(kb.input.text(), "/kb adopt calc");
    kb.update(key(KeyCode::Enter));
    assert!(
        matches!(kb.overlay, Some(Overlay::Confirm(_))),
        "calc is connected and unmanaged: the adopt confirmation opens"
    );
}

#[test]
fn without_suggestions_the_old_key_behaviour_is_unchanged() {
    // Tab still toggles the focused tool card.
    let mut app = fresh();
    agent_event(
        &mut app,
        harness_tui::AgentUiEvent::ToolCallStarted {
            call_id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({}),
        },
    );
    type_text(&mut app, "plain text");
    assert!(app.suggestions().is_none());
    app.update(key(KeyCode::Tab));
    assert!(matches!(&app.transcript[0], Entry::ToolCard(c) if c.expanded));
    assert_eq!(
        app.input.text(),
        "plain text",
        "Tab did not touch the input"
    );

    // Up/Down still move the cursor in a multi-line input, even when it starts with a slash.
    let mut multi = fresh();
    type_text(&mut multi, "/mo");
    multi.update(alt_enter());
    type_text(&mut multi, "x");
    assert!(multi.suggestions().is_none(), "multi-line input has none");
    multi.update(key(KeyCode::Up));
    assert_eq!(multi.input.cursor_row_col().0, 0);
    multi.update(key(KeyCode::Down));
    assert_eq!(multi.input.cursor_row_col().0, 1);
}

#[test]
fn suggestions_hide_while_an_overlay_is_open_or_the_cursor_is_not_at_the_end() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    app.update(key(KeyCode::Left));
    assert!(app.suggestions().is_none(), "the cursor is mid-text");
    app.update(key(KeyCode::End));
    assert!(app.suggestions().is_some());
    app.input.clear();
    say(&mut app, "/help");
    assert!(app.overlay.is_some());
    type_text(&mut app, "/mo");
    assert!(app.suggestions().is_none());
}

#[test]
fn modified_keys_are_not_taken_by_the_suggestions() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    app.update(alt_enter());
    assert_eq!(
        app.input.text(),
        "/mo\n",
        "Alt+Enter still inserts a newline"
    );
    let mut shift = fresh();
    type_text(&mut shift, "/mo");
    shift.update(Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));
    assert_eq!(shift.input.text(), "/mo");
}

// -------------------------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------------------------

#[test]
fn the_popup_sits_directly_above_the_input_box_and_clears_the_status_bar() {
    let mut app = fresh();
    for i in 0..30 {
        ui_event(
            &mut app,
            UiEvent::Notice(format!("note {i:02} in the transcript")),
        );
    }
    settle(&mut app);
    let plain = draw(&app, 100, 30);
    let (nx, ny) = find(&plain, "note 08").expect("notice near the top");
    let notice_fg = plain[(nx, ny)].fg;

    type_text(&mut app, "/mo");
    let buffer = draw(&app, 100, 30);
    let t = text(&buffer);
    let rows: Vec<&str> = t.lines().collect();
    let (_, input_top) = find(&buffer, "❯ /mo").expect("input row");
    let input_top = input_top - 1;
    assert!(rows[usize::from(input_top)].starts_with("╭"), "{t}");
    // The popup's bottom border is on the row directly above the input's top border.
    let bottom = rows[usize::from(input_top) - 1];
    assert!(bottom.contains('╰') && bottom.contains('╯'), "{t}");
    let (cx, cy) = find(&buffer, "› /model").expect("selected suggestion");
    assert!(
        cy < input_top - 1,
        "inside the popup, above its bottom border"
    );
    assert_eq!(buffer[(cx, cy)].bg, app.theme.resolve(app.theme.highlight));
    assert!(t.contains("pick a model, or switch to one directly"), "{t}");
    // The status bar is untouched.
    let status = rows.last().expect("status");
    assert!(
        status.contains("luna/max") && status.contains("MCP 0/0"),
        "{status}"
    );
    // No backdrop: transcript text outside the popup keeps its colour.
    assert_eq!(buffer[(nx, ny)].symbol(), "n");
    assert_eq!(
        buffer[(nx, ny)].fg,
        notice_fg,
        "the transcript is not dimmed"
    );
}

#[test]
fn the_typed_prefix_is_highlighted_and_the_rest_is_ghost_text_after_the_cursor() {
    let mut app = fresh();
    type_text(&mut app, "/mo");
    let buffer = draw(&app, 100, 30);
    let theme = &app.theme;
    // Ghost text `del` directly after `/mo` in the input.
    let (px, py) = find(&buffer, "❯ /mo").expect("input");
    let ghost_x = px + "❯ /mo".chars().count() as u16;
    assert_eq!(buffer[(ghost_x, py)].symbol(), "d");
    assert_eq!(
        buffer[(ghost_x, py)].fg,
        theme.resolve(theme.muted),
        "dim ghost text"
    );
    assert_eq!(buffer[(ghost_x + 2, py)].symbol(), "l");
    // The popup row of a non-selected suggestion highlights the typed prefix.
    let mut m = fresh();
    type_text(&mut m, "/m");
    let buffer = draw(&m, 100, 30);
    let (x, y) = find(&buffer, "/mcp").expect("second row");
    assert_eq!(
        buffer[(x, y)].fg,
        m.theme.resolve(m.theme.accent),
        "typed prefix"
    );
    assert!(
        buffer[(x, y)]
            .modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    assert_eq!(
        buffer[(x + 2, y)].fg,
        m.theme.resolve(m.theme.text),
        "rest of the name"
    );
    assert!(
        !draw(&m, 100, 30)
            .content()
            .iter()
            .any(|c| c.modifier.contains(ratatui::style::Modifier::REVERSED))
    );
}

#[test]
fn the_popup_shows_at_most_eight_rows_and_scrolls_with_the_selection() {
    let mut app = fresh();
    ui_event(&mut app, UiEvent::Notice("something happened".to_owned()));
    type_text(&mut app, "/");
    let first = text(&draw(&app, 100, 30));
    assert!(
        first.contains("/model") && first.contains("/clear"),
        "{first}"
    );
    assert!(
        !first.contains("/quit"),
        "only {MAX_ROWS} rows at once:\n{first}"
    );
    assert!(first.contains("1/10"), "position indicator:\n{first}");
    for _ in 0..9 {
        app.update(key(KeyCode::Down));
    }
    let last = text(&draw(&app, 100, 30));
    assert!(
        last.contains("/quit"),
        "the selection scrolled into view:\n{last}"
    );
    assert!(
        !last.contains("/model "),
        "the first row scrolled out:\n{last}"
    );
    assert!(last.contains("10/10"), "{last}");
}

#[test]
fn suggestions_survive_small_terminals_and_never_overlap_the_status_bar() {
    let mut app = fresh();
    type_text(&mut app, "/");
    for (w, h) in [
        (80, 24),
        (40, 10),
        (20, 6),
        (12, 5),
        (9, 4),
        (3, 3),
        (1, 1),
        (100, 8),
    ] {
        let buffer = draw(&app, w, h);
        let t = text(&buffer);
        if h >= 4 && w >= 12 {
            assert!(
                t.lines().last().expect("status").contains("luna/max") || w < 20,
                "{w}x{h}:\n{t}"
            );
        }
    }
    // Model aliases and servers render too.
    let mut other = app_with_servers();
    type_text(&mut other, "/mcp connect ");
    let t = text(&draw(&other, 100, 30));
    assert!(
        t.contains("calc") && t.contains("connected, 3 tools, not in the KB"),
        "{t}"
    );
    assert!(t.contains("servers"), "{t}");
}

#[test]
fn theme_autocompletes_and_theme_arguments_complete_slugs() {
    let mut app = fresh();
    type_text(&mut app, "/the");
    assert_eq!(inserts(&app), ["/theme"]);
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/theme ");

    let mut app = fresh();
    type_text(&mut app, "/theme gru");
    assert_eq!(
        inserts(&app),
        ["gruvbox-dark", "gruvbox-dark-hard", "gruvbox-light"]
    );
}
