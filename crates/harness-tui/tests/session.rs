//! Terminal lifecycle tests: guard restoration on normal exit, returned errors and panics, and
//! the event loop against a scripted input source, a scripted agent port and a `TestBackend`.

mod common;

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use harness_agent::events::StopReason;
use harness_tui::{
    AgentUiEvent, Entry, InputSource, ScriptedAgentHandle, TerminalGuard, TerminalRestore,
    TuiError, UiCommand, UiEvent, event_loop, run_session,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Default)]
struct CountingRestore(Arc<AtomicUsize>);

impl CountingRestore {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl TerminalRestore for CountingRestore {
    fn restore(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ScriptedInput(VecDeque<io::Result<Option<Event>>>);

impl ScriptedInput {
    /// Type each line followed by Enter.
    fn lines(lines: &[&str]) -> Self {
        let mut events = VecDeque::new();
        for line in lines {
            for c in line.chars() {
                events.push_back(Ok(Some(key_event(KeyCode::Char(c)))));
            }
            events.push_back(Ok(Some(key_event(KeyCode::Enter))));
        }
        Self(events)
    }

    fn push(&mut self, event: io::Result<Option<Event>>) {
        self.0.push_back(event);
    }
}

fn key_event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl_c() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
}

impl InputSource for ScriptedInput {
    fn next_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        self.0
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::UnexpectedEof, "script ended")))
    }
}

fn test_terminal() -> io::Result<Terminal<TestBackend>> {
    Terminal::new(TestBackend::new(100, 30)).map_err(|e| io::Error::other(e.to_string()))
}

#[test]
fn guard_restores_exactly_once_on_drop() {
    let restore = CountingRestore::default();
    {
        let _guard = TerminalGuard::new(restore.clone());
        assert_eq!(restore.count(), 0);
    }
    assert_eq!(restore.count(), 1);
}

#[test]
fn guard_restores_while_a_panic_unwinds() {
    let restore = CountingRestore::default();
    let r = restore.clone();
    let result = catch_unwind(AssertUnwindSafe(move || {
        let _guard = TerminalGuard::new(r);
        panic!("boom inside the TUI");
    }));
    assert!(result.is_err());
    assert_eq!(restore.count(), 1);
}

#[test]
fn a_panic_inside_the_loop_restores_the_terminal() {
    struct PanickingInput;
    impl InputSource for PanickingInput {
        fn next_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
            panic!("input blew up");
        }
    }
    let restore = CountingRestore::default();
    let r = restore.clone();
    let result = catch_unwind(AssertUnwindSafe(move || {
        let mut app = fresh();
        let mut port = ScriptedAgentHandle::new();
        let _ = run_session(test_terminal, r, &mut PanickingInput, &mut app, &mut port);
    }));
    assert!(result.is_err());
    assert_eq!(restore.count(), 1);
}

#[test]
fn quitting_restores_the_terminal_once_and_shuts_the_agent_down() {
    let restore = CountingRestore::default();
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    let mut input = ScriptedInput::lines(&["/quit"]);
    let result = run_session(
        test_terminal,
        restore.clone(),
        &mut input,
        &mut app,
        &mut port,
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        restore.count(),
        1,
        "the terminal-restore hook ran exactly once"
    );
    assert!(app.should_quit());
    assert_eq!(
        handle.shutdown_count(),
        1,
        "the agent runtime was asked to stop"
    );
}

#[test]
fn ctrl_c_twice_quits_through_the_loop() {
    let restore = CountingRestore::default();
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let mut input = ScriptedInput(VecDeque::from([Ok(Some(ctrl_c())), Ok(Some(ctrl_c()))]));
    let result = run_session(
        test_terminal,
        restore.clone(),
        &mut input,
        &mut app,
        &mut port,
    );
    assert!(result.is_ok(), "{result:?}");
    assert!(app.should_quit());
    assert_eq!(restore.count(), 1);
}

#[test]
fn an_input_error_restores_the_terminal_stops_the_agent_and_is_returned() {
    let restore = CountingRestore::default();
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    let mut input = ScriptedInput(VecDeque::from([Err(io::Error::other("tty lost"))]));
    let result = run_session(
        test_terminal,
        restore.clone(),
        &mut input,
        &mut app,
        &mut port,
    );
    assert!(matches!(result, Err(TuiError::Input(_))), "{result:?}");
    assert_eq!(restore.count(), 1);
    assert_eq!(handle.shutdown_count(), 1);
    assert!(result.unwrap_err().to_string().contains("tty lost"));
}

#[test]
fn an_init_failure_still_restores_and_stops_the_agent() {
    let restore = CountingRestore::default();
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    let mut input = ScriptedInput::lines(&["/quit"]);
    let result = run_session::<TestBackend, _, _>(
        || Err(io::Error::other("not a terminal")),
        restore.clone(),
        &mut input,
        &mut app,
        &mut port,
    );
    assert!(matches!(result, Err(TuiError::Init(_))), "{result:?}");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cannot initialize the terminal")
    );
    assert_eq!(
        restore.count(),
        1,
        "raw mode may be half enabled, so restore anyway"
    );
    assert_eq!(handle.shutdown_count(), 1);
}

#[test]
fn a_scripted_conversation_runs_through_the_loop() {
    let restore = CountingRestore::default();
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    handle.set_responder(|command| match command {
        UiCommand::SendUser(text) => vec![
            UiEvent::Agent(AgentUiEvent::TurnStarted),
            UiEvent::Agent(AgentUiEvent::TextDelta(format!("you said: {text}"))),
            UiEvent::Agent(AgentUiEvent::TurnFinished {
                reason: StopReason::Completed,
            }),
        ],
        _ => vec![],
    });
    let mut input = ScriptedInput::lines(&["hello"]);
    input.push(Ok(None)); // an idle tick lets the loop deliver the reply
    input.0.extend(ScriptedInput::lines(&["/quit"]).0);
    let result = run_session(
        test_terminal,
        restore.clone(),
        &mut input,
        &mut app,
        &mut port,
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(handle.sent(), vec![UiCommand::SendUser("hello".to_owned())]);
    assert!(app.transcript.contains(&Entry::User("hello".to_owned())));
    assert!(app.transcript.contains(&Entry::Assistant {
        text: "you said: hello".to_owned(),
        streaming: false
    }));
    assert!(!app.busy);
    assert_eq!(restore.count(), 1);
}

#[test]
fn the_loop_drains_agent_events_every_tick_and_redraws() {
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    handle.push(UiEvent::Notice("pushed before the first tick".to_owned()));
    let mut terminal = test_terminal().expect("terminal");
    let mut input = ScriptedInput(VecDeque::from([
        Ok(None),
        Ok(Some(key_event(KeyCode::Char('h')))),
        Ok(Some(ctrl_c())),
        Ok(Some(ctrl_c())),
    ]));
    event_loop(&mut terminal, &mut app, &mut port, &mut input).expect("loop");
    assert!(
        app.transcript
            .contains(&Entry::Notice("pushed before the first tick".to_owned()))
    );
    let screen = text(terminal.backend().buffer());
    assert!(screen.contains("pushed before the first tick"), "{screen}");
    assert_eq!(
        app.input.text(),
        "h",
        "the key typed between ticks reached the input"
    );
}

#[test]
fn pasted_text_reaches_the_input_and_key_releases_are_ignored() {
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let mut release = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    let mut input = ScriptedInput(VecDeque::from([
        Ok(Some(Event::Paste("first\nsecond".to_owned()))),
        Ok(Some(Event::Key(release))),
        Ok(Some(Event::Resize(80, 24))),
        Ok(Some(ctrl_c())),
        Ok(Some(ctrl_c())),
    ]));
    let mut terminal = test_terminal().expect("terminal");
    event_loop(&mut terminal, &mut app, &mut port, &mut input).expect("loop");
    assert_eq!(app.input.text(), "first\nsecond");
    let screen = text(terminal.backend().buffer());
    assert!(
        screen.contains("first") && screen.contains("second"),
        "{screen}"
    );
}

#[test]
fn approvals_travel_from_the_port_to_the_screen_and_the_answer_goes_back() {
    let mut app = fresh();
    let mut port = ScriptedAgentHandle::new();
    let handle = port.handle();
    handle.push(UiEvent::ApprovalNeeded {
        id: 11,
        request: harness_agent::approval::ApprovalRequest {
            id: 11,
            tool: "run_shell".to_owned(),
            class: harness_agent::approval::ApprovalClass::Shell,
            summary: "Run: ls".to_owned(),
            diff: None,
        },
    });
    let mut terminal = test_terminal().expect("terminal");
    let mut input = ScriptedInput(VecDeque::from([
        Ok(None),
        Ok(Some(key_event(KeyCode::Enter))), // Enter must not approve
        Ok(Some(key_event(KeyCode::Char('n')))),
        Ok(Some(ctrl_c())),
        Ok(Some(ctrl_c())),
    ]));
    event_loop(&mut terminal, &mut app, &mut port, &mut input).expect("loop");
    assert_eq!(
        handle.sent(),
        vec![UiCommand::Approve {
            id: 11,
            decision: harness_agent::approval::Decision::Deny
        }]
    );
}
