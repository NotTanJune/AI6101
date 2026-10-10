//! Animations: every one is driven by `App::frame`, so tests step frames and never wait.

mod common;

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use common::*;
use harness_agent::model::TokenUsage;
use harness_tui::anim::{self, SPINNER};
use harness_tui::terminal::TICK_MS;
use harness_tui::{
    AgentUiEvent, App, CTRL_C_WINDOW_TICKS, CostSnapshot, InputSource, McpServerStatus, McpState,
    Msg, ScriptedAgentHandle, TICK, UiEvent, event_loop,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

fn tick(app: &mut App) {
    app.update(Msg::Tick);
}

fn rgb(color: Color) -> (u8, u8, u8) {
    let Color::Rgb(r, g, b) = color else {
        panic!("true colour expected, got {color:?}");
    };
    (r, g, b)
}

fn distance(a: Color, b: Color) -> u32 {
    let (a, b) = (rgb(a), rgb(b));
    u32::from(a.0.abs_diff(b.0)) + u32::from(a.1.abs_diff(b.1)) + u32::from(a.2.abs_diff(b.2))
}

// -------------------------------------------------------------------------------------------
// Tick rate and the frame counter
// -------------------------------------------------------------------------------------------

#[test]
fn the_tick_is_about_thirty_frames_per_second() {
    assert_eq!(TICK, Duration::from_millis(TICK_MS));
    assert!((30..=34).contains(&TICK_MS), "{TICK_MS}");
}

#[test]
fn the_ctrl_c_window_is_still_about_one_second_of_wall_clock_time() {
    let window_ms = u64::from(CTRL_C_WINDOW_TICKS) * TICK_MS;
    assert!(
        (900..=1100).contains(&window_ms),
        "{CTRL_C_WINDOW_TICKS} ticks of {TICK_MS} ms are {window_ms} ms"
    );
    // The window closes after exactly that many ticks.
    let mut app = fresh();
    app.update(ctrl('c'));
    for _ in 0..CTRL_C_WINDOW_TICKS {
        tick(&mut app);
    }
    app.update(ctrl('c'));
    assert!(!app.should_quit(), "the second press came too late");
    let mut quick = fresh();
    quick.update(ctrl('c'));
    for _ in 0..CTRL_C_WINDOW_TICKS - 1 {
        tick(&mut quick);
    }
    quick.update(ctrl('c'));
    assert!(quick.should_quit(), "inside the window");
}

#[test]
fn only_ticks_advance_the_frame_counter() {
    let mut app = fresh();
    assert_eq!(app.frame, 0);
    type_text(&mut app, "hello");
    ui_event(&mut app, UiEvent::Notice("x".to_owned()));
    assert_eq!(app.frame, 0, "keys and events do not move animations");
    for expected in 1..=5 {
        tick(&mut app);
        assert_eq!(app.frame, expected);
    }
}

struct Counting {
    events: VecDeque<Option<Event>>,
    timeouts: Vec<Duration>,
}

impl InputSource for Counting {
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        self.timeouts.push(timeout);
        Ok(self.events.pop_front().unwrap_or_else(|| {
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
        }))
    }
}

#[test]
fn the_loop_ticks_by_the_clock_not_by_the_number_of_events() {
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let mut events: VecDeque<Option<Event>> = (0..120)
        .map(|_| {
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )))
        })
        .collect();
    events.push_back(Some(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ))));
    events.push_back(Some(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ))));
    let mut input = Counting {
        events,
        timeouts: Vec::new(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    event_loop(&mut terminal, &mut app, &mut port, &mut input).expect("loop");
    assert_eq!(app.input.text().len(), 120);
    assert!(
        app.frame < 60,
        "122 events must not mean 122 frames, got {}",
        app.frame
    );
    assert!(
        input.timeouts.iter().all(|t| *t <= TICK),
        "the poll never waits longer than one tick"
    );
}

#[test]
fn an_idle_settled_app_is_not_animating_and_a_busy_one_is() {
    let mut app = fresh();
    assert!(app.is_animating(), "the welcome banner animates at first");
    settle(&mut app);
    assert!(!app.is_animating());
    say(&mut app, "hi");
    assert!(app.is_animating(), "a running turn animates the spinner");
    agent_event(
        &mut app,
        AgentUiEvent::TurnFinished {
            reason: harness_agent::events::StopReason::Completed,
        },
    );
    settle(&mut app);
    assert!(!app.is_animating());
}

// -------------------------------------------------------------------------------------------
// Spinner, cursor, dot
// -------------------------------------------------------------------------------------------

#[test]
fn the_busy_input_title_shows_a_spinner_that_advances_and_changes_colour() {
    let mut app = fresh();
    say(&mut app, "hello");
    let mut seen = std::collections::BTreeSet::new();
    let mut colours = std::collections::BTreeSet::new();
    for frame in 0..40 {
        let buffer = draw(&app, 100, 24);
        let t = text(&buffer);
        let glyph = SPINNER
            .iter()
            .find(|g| t.contains(&format!("╭ {g} working…")))
            .unwrap_or_else(|| panic!("frame {frame}: no spinner in\n{t}"));
        seen.insert(*glyph);
        let (x, y) = find(&buffer, glyph).expect("glyph");
        colours.insert(rgb(buffer[(x, y)].fg));
        tick(&mut app);
    }
    assert_eq!(seen.len(), SPINNER.len(), "every braille frame appears");
    assert!(colours.len() > 5, "the colour cycles along the gradient");
    assert!(app.busy);
}

#[test]
fn running_tool_cards_show_the_spinner_and_finished_ones_do_not() {
    let mut app = fresh();
    agent_event(
        &mut app,
        AgentUiEvent::ToolCallStarted {
            call_id: "c1".to_owned(),
            name: "grep".to_owned(),
            arguments: serde_json::json!({}),
        },
    );
    let first = text(&draw(&app, 100, 24));
    assert!(first.contains("grep({})  ⠋ running"), "{first}");
    tick(&mut app);
    tick(&mut app);
    let later = text(&draw(&app, 100, 24));
    assert!(later.contains("grep({})  ⠙ running"), "{later}");
    agent_event(
        &mut app,
        AgentUiEvent::ToolCallFinished {
            call_id: "c1".to_owned(),
            output: "ok".to_owned(),
            is_error: false,
            duration_ms: 3,
        },
    );
    let done = text(&draw(&app, 100, 24));
    assert!(
        done.contains("✓ 3ms") && !done.contains("running"),
        "{done}"
    );
}

#[test]
fn the_streaming_cursor_pulses_but_never_disappears() {
    let mut app = fresh();
    say(&mut app, "hello");
    agent_event(&mut app, AgentUiEvent::TextDelta("typing".to_owned()));
    let mut colours = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let buffer = draw(&app, 80, 24);
        let (x, y) = find(&buffer, "█").expect("the cursor block is always there");
        colours.insert(rgb(buffer[(x, y)].fg));
        tick(&mut app);
    }
    assert!(colours.len() > 4, "the cursor colour pulses: {colours:?}");
    // The pulse is periodic: the same frame gives the same colour.
    let a = draw(&app, 80, 24);
    for _ in 0..20 {
        tick(&mut app);
    }
    let b = draw(&app, 80, 24);
    let (x, y) = find(&a, "█").expect("cursor");
    assert_eq!(a[(x, y)].fg, b[(x, y)].fg);
}

fn connected(app: &mut App) {
    ui_event(
        app,
        UiEvent::McpStatus(McpServerStatus {
            name: "calc".to_owned(),
            state: McpState::Connected,
            live_tools: 2,
            managed: true,
            exposed: vec![],
        }),
    );
}

#[test]
fn the_mcp_dot_pulses_only_while_a_tool_call_is_in_flight() {
    let mut app = App::new(app_init(
        "luna",
        harness_agent::registry::Effort::Max,
        &["calc"],
    ));
    connected(&mut app);
    let dot_fg = |app: &App| {
        let buffer = draw(app, 100, 24);
        let (x, y) = find(&buffer, "●").expect("dot");
        buffer[(x, y)].fg
    };
    let still: std::collections::BTreeSet<_> = (0..30)
        .map(|_| {
            tick(&mut app);
            rgb(dot_fg(&app))
        })
        .collect();
    assert_eq!(still.len(), 1, "a calm dot when nothing runs");
    assert_eq!(dot_fg(&app), app.theme.resolve(app.theme.success));
    agent_event(
        &mut app,
        AgentUiEvent::ToolCallStarted {
            call_id: "c1".to_owned(),
            name: "mcp__calc__add".to_owned(),
            arguments: serde_json::json!({}),
        },
    );
    let pulsing: std::collections::BTreeSet<_> = (0..30)
        .map(|_| {
            tick(&mut app);
            rgb(dot_fg(&app))
        })
        .collect();
    assert!(pulsing.len() > 4, "{pulsing:?}");
}

// -------------------------------------------------------------------------------------------
// Smooth scrolling
// -------------------------------------------------------------------------------------------

fn long_transcript() -> App {
    let mut app = fresh();
    for i in 0..80 {
        ui_event(&mut app, UiEvent::Notice(format!("note {i:02}")));
    }
    app
}

#[test]
fn page_keys_set_a_target_and_the_view_glides_there_exactly() {
    let mut app = long_transcript();
    app.update(key(KeyCode::PageUp));
    assert_eq!(app.scroll.from_bottom, 10, "the target is set at once");
    assert_eq!(app.scroll.shown, 0, "but the view has not moved yet");
    let mut last = 0;
    let mut frames = 0;
    while app.scroll.shown != app.scroll.from_bottom {
        tick(&mut app);
        assert!(
            app.scroll.shown > last,
            "monotonic: {} after {last}",
            app.scroll.shown
        );
        assert!(app.scroll.shown <= 10, "never overshoots");
        last = app.scroll.shown;
        frames += 1;
        assert!(frames < 20);
    }
    assert!(frames > 1, "it takes more than one frame: no jump");
    assert_eq!(app.scroll.shown, 10, "ends exactly on the target");
    tick(&mut app);
    assert_eq!(app.scroll.shown, 10, "and stays there");
}

#[test]
fn paging_back_down_glides_down_without_overshoot() {
    let mut app = long_transcript();
    for _ in 0..3 {
        app.update(key(KeyCode::PageUp));
    }
    settle(&mut app);
    assert_eq!(app.scroll.shown, 30);
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    let mut last = 30;
    while app.scroll.shown != 0 {
        tick(&mut app);
        assert!(app.scroll.shown < last);
        last = app.scroll.shown;
    }
    assert_eq!(app.scroll.from_bottom, 0);
}

#[test]
fn the_drawn_offset_follows_the_eased_value() {
    let mut app = long_transcript();
    let pinned = text(&draw(&app, 60, 12));
    assert!(pinned.contains("note 79"), "{pinned}");
    app.update(key(KeyCode::PageUp));
    let before = text(&draw(&app, 60, 12));
    assert_eq!(before, pinned, "nothing moves before the first tick");
    tick(&mut app);
    let mid = text(&draw(&app, 60, 12));
    assert_ne!(mid, pinned, "the first frame already moved");
    settle(&mut app);
    let after = text(&draw(&app, 60, 12));
    assert!(
        !after.contains("note 79") && after.contains("note 6"),
        "{after}"
    );
}

#[test]
fn new_output_keeps_a_pinned_view_pinned() {
    let mut app = long_transcript();
    settle(&mut app);
    for i in 80..90 {
        ui_event(&mut app, UiEvent::Notice(format!("note {i:02}")));
        tick(&mut app);
        assert_eq!(app.scroll.shown, 0);
        let screen = text(&draw(&app, 60, 12));
        assert!(screen.contains(&format!("note {i:02}")), "{screen}");
    }
    // Sending a message while scrolled up glides back to the bottom.
    app.update(key(KeyCode::PageUp));
    settle(&mut app);
    assert_eq!(app.scroll.shown, 10);
    say(&mut app, "hello");
    assert_eq!(app.scroll.from_bottom, 0);
    settle(&mut app);
    assert_eq!(app.scroll.shown, 0);
}

// -------------------------------------------------------------------------------------------
// Status numbers
// -------------------------------------------------------------------------------------------

#[test]
fn cost_and_token_numbers_roll_up_to_their_exact_final_values() {
    let mut app = fresh();
    agent_event(
        &mut app,
        AgentUiEvent::Usage {
            model_alias: "luna".to_owned(),
            usage: TokenUsage {
                input: Some(48_000),
                output_including_reasoning: Some(1_200),
                ..TokenUsage::default()
            },
            cost_usd: Some(1.5),
        },
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
    assert_eq!(app.display.tokens_in, 0, "numbers start from the old value");
    let (mut last_in, mut last_out, mut last_spent) = (0, 0, 0.0);
    let mut frames = 0;
    while app.display.tokens_in != 48_000
        || app.display.tokens_out != 1_200
        || app.display.spent_usd != 1.5
    {
        tick(&mut app);
        assert!(app.display.tokens_in >= last_in && app.display.tokens_in <= 48_000);
        assert!(app.display.tokens_out >= last_out && app.display.tokens_out <= 1_200);
        assert!(app.display.spent_usd >= last_spent && app.display.spent_usd <= 1.5);
        (last_in, last_out, last_spent) = (
            app.display.tokens_in,
            app.display.tokens_out,
            app.display.spent_usd,
        );
        frames += 1;
        assert!(frames < 80, "a few dozen frames at most");
    }
    assert!(frames > 3, "it rolls over several frames");
    let status = text(&draw(&app, 100, 24));
    let last = status.lines().last().expect("status");
    assert!(last.contains("tokens in 48.0k out 1.2k"), "{last}");
    assert!(last.contains("$1.5000 / $5.00"), "{last}");
}

#[test]
fn a_mid_roll_frame_shows_an_intermediate_number() {
    let mut app = fresh();
    ui_event(
        &mut app,
        UiEvent::Cost(CostSnapshot {
            spent_usd: 2.0,
            unreconciled_usd: 0.0,
            remaining_usd: 3.0,
            cap_usd: 5.0,
        }),
    );
    tick(&mut app);
    let t = text(&draw(&app, 100, 24));
    let status = t.lines().last().expect("status");
    assert!(
        status.contains("$0.8000") && !status.contains("$2.0000"),
        "{status}"
    );
}

// -------------------------------------------------------------------------------------------
// Popup and banner fades
// -------------------------------------------------------------------------------------------

#[test]
fn the_popup_backdrop_dims_in_gradually_and_ends_at_a_fixed_level() {
    let mut app = fresh();
    ui_event(
        &mut app,
        UiEvent::Notice("a notice in the corner".to_owned()),
    );
    let plain = draw(&app, 120, 40);
    let (x, y) = find(&plain, "a notice in the corner").expect("notice");
    say(&mut app, "/model");
    let bg = app.theme.resolve(app.theme.bg);
    let mut last = distance(plain[(x, y)].fg, bg);
    let mut distances = vec![last];
    for _ in 0..8 {
        let buffer = draw(&app, 120, 40);
        let d = distance(buffer[(x, y)].fg, bg);
        assert!(d <= last, "the text only gets dimmer: {d} after {last}");
        last = d;
        distances.push(d);
        tick(&mut app);
    }
    assert!(
        distances[0] > *distances.last().expect("last"),
        "{distances:?}"
    );
    let settled = draw(&app, 120, 40)[(x, y)].fg;
    tick(&mut app);
    assert_eq!(
        draw(&app, 120, 40)[(x, y)].fg,
        settled,
        "stable once settled"
    );
}

#[test]
fn the_welcome_banner_fades_in_shimmers_and_settles() {
    let mut app = fresh();
    let bg = app.theme.resolve(app.theme.bg);
    let title_fg = |app: &App| {
        let buffer = draw(app, 100, 24);
        let (x, y) = find(&buffer, "mcpe").expect("title");
        buffer[(x, y)].fg
    };
    let start = distance(title_fg(&app), bg);
    let mut last = start;
    for _ in 0..16 {
        tick(&mut app);
        let d = distance(title_fg(&app), bg);
        assert!(d >= last, "the title only gets brighter while it fades in");
        last = d;
    }
    assert!(
        last > start + 100,
        "clearly visible after the fade: {start} -> {last}"
    );
    // After the shimmer the colours no longer change.
    for _ in 0..60 {
        tick(&mut app);
    }
    let settled = draw(&app, 100, 24);
    tick(&mut app);
    assert_eq!(
        draw(&app, 100, 24),
        settled,
        "a settled banner is a stable snapshot"
    );
    let _ = anim::BANNER_FADE_FRAMES;
}
