//! Automated stand-in for the manual smoke test (plan 003, step 7).
//!
//! A real `AgentHandle` (own thread and runtime) runs over `MockModelClient::from_fixture_dir`
//! loaded from `fixtures/agent/hello`, and the terminal loop renders into a `TestBackend`.
//! Scenario: hello -> streamed reply; a second turn calls `write_file` -> approval -> `n` ->
//! the card shows the denial; `/model sol` then `/effort none` -> an error listing the allowed
//! efforts; Ctrl-C twice quits. Finally the session directory must hold `transcript.jsonl`.
//!
//! This is not a human typing into a real terminal: the interactive smoke stays a manual check.

mod common;

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use harness_agent::registry::Effort;
use harness_core::cost::CostLedger;
use harness_tui::{
    AgentPort, AgentUiEvent, App, Entry, InputSource, Msg, UiCommand, UiEvent, drive, event_loop,
    ui,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

const WIDTH: u16 = 110;
const HEIGHT: u16 = 40;

fn hello_script() -> Script {
    Script::Dir(repo_root().join("fixtures/agent/hello"))
}

fn finished_turns(events: &[UiEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, UiEvent::Agent(AgentUiEvent::TurnFinished { .. })))
        .count()
}

fn has_approval(events: &[UiEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, UiEvent::ApprovalNeeded { .. }))
}

fn transcript_lines(session_dir: &Path) -> Vec<serde_json::Value> {
    let path = session_dir.join("transcript.jsonl");
    let text = std::fs::read_to_string(&path).expect("transcript.jsonl exists in the session dir");
    text.lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect()
}

fn assert_transcript(session_dir: &Path) {
    let lines = transcript_lines(session_dir);
    assert!(!lines.is_empty(), "the transcript has lines");
    for line in &lines {
        assert_eq!(line["schema"], "agent-session/1", "{line}");
    }
    let raw = std::fs::read_to_string(session_dir.join("transcript.jsonl")).expect("read");
    assert!(
        raw.lines()
            .all(|l| l.contains("\"schema\":\"agent-session/1\"")),
        "{raw}"
    );
    let kinds: Vec<&str> = lines.iter().filter_map(|l| l["kind"].as_str()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "user_message").count(),
        2,
        "{kinds:?}"
    );
    for kind in [
        "session_start",
        "assistant_message",
        "tool_call",
        "approval",
        "tool_result",
        "model_switch",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    let approval = lines
        .iter()
        .find(|l| l["kind"] == "approval")
        .expect("approval line");
    assert_eq!(approval["decision"], "deny");
    assert_eq!(approval["tool"], "write_file");
    let switch = lines
        .iter()
        .find(|l| l["kind"] == "model_switch")
        .expect("model switch");
    assert_eq!(switch["model_alias"], "sol");
    assert_eq!(switch["effort"], "xhigh");
}

fn assert_costs(ledger: &Mutex<CostLedger>) {
    // Three calls on luna: (120+160+210) input and (24+40+12) output tokens.
    let spent = ledger.lock().expect("ledger").spent_usd();
    let expected = (490.0 * 0.1 + 76.0 * 0.5) / 1e6;
    assert!(
        (spent - expected).abs() < 1e-9,
        "spent {spent}, expected {expected}"
    );
}

// -------------------------------------------------------------------------------------------
// Variant A: the real `event_loop` with a scripted input source
// -------------------------------------------------------------------------------------------

/// Delegates to a real port and records every event it hands to the loop.
struct ObservingPort<P: AgentPort> {
    inner: P,
    seen: Arc<Mutex<Vec<UiEvent>>>,
    sent: Arc<Mutex<Vec<UiCommand>>>,
}

impl<P: AgentPort> AgentPort for ObservingPort<P> {
    fn send(&mut self, command: UiCommand) {
        self.sent.lock().expect("lock").push(command.clone());
        self.inner.send(command);
    }

    fn poll(&mut self) -> Vec<UiEvent> {
        let events = self.inner.poll();
        self.seen
            .lock()
            .expect("lock")
            .extend(events.iter().cloned());
        events
    }

    fn shutdown(&mut self, timeout: Duration) -> bool {
        self.inner.shutdown(timeout)
    }
}

enum Step {
    Type(&'static str),
    Press(KeyCode),
    CtrlC,
    /// Idle ticks until the observed events satisfy the predicate.
    Wait(&'static str, fn(&[UiEvent]) -> bool),
    /// A few idle ticks so the loop redraws.
    Settle,
}

struct ReactiveInput {
    steps: VecDeque<Step>,
    queued: VecDeque<Event>,
    seen: Arc<Mutex<Vec<UiEvent>>>,
    deadline: Instant,
    settle_left: u32,
}

impl InputSource for ReactiveInput {
    fn next_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
            match self.steps.front() {
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "script ended")),
                Some(Step::Type(text)) => {
                    for c in text.chars() {
                        let code = if c == '\n' {
                            KeyCode::Enter
                        } else {
                            KeyCode::Char(c)
                        };
                        self.queued
                            .push_back(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                    }
                    self.steps.pop_front();
                }
                Some(Step::Press(code)) => {
                    self.queued
                        .push_back(Event::Key(KeyEvent::new(*code, KeyModifiers::NONE)));
                    self.steps.pop_front();
                }
                Some(Step::CtrlC) => {
                    self.queued.push_back(Event::Key(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )));
                    self.steps.pop_front();
                }
                Some(Step::Settle) => {
                    if self.settle_left == 0 {
                        self.settle_left = 3;
                    }
                    self.settle_left -= 1;
                    if self.settle_left == 0 {
                        self.steps.pop_front();
                    }
                    return Ok(None);
                }
                Some(Step::Wait(what, done)) => {
                    if done(&self.seen.lock().expect("lock")) {
                        self.steps.pop_front();
                        continue;
                    }
                    if Instant::now() > self.deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("timed out waiting for {what}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    return Ok(None);
                }
            }
        }
    }
}

#[test]
fn the_scenario_through_the_real_event_loop_with_a_scripted_input_source() {
    let f = spawn(SetupOpts::new(hello_script()));
    let mut app = f.app();
    let Fixture {
        handle,
        workspace,
        session_dir,
        ledger,
        ..
    } = f;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let steps = VecDeque::from([
        Step::Type("hello\n"),
        Step::Wait("the first turn", |e| finished_turns(e) >= 1),
        Step::Type("please create hello.txt\n"),
        Step::Wait("the approval request", has_approval),
        Step::Press(KeyCode::Char('n')),
        Step::Wait("the second turn", |e| finished_turns(e) >= 2),
        Step::Type("/model sol\n"),
        Step::Wait("the model switch", |e| {
            e.iter()
                .any(|e| matches!(e, UiEvent::ModelChanged { alias, .. } if alias == "sol"))
        }),
        Step::Type("/effort none\n"),
        Step::Settle,
        Step::CtrlC,
        Step::CtrlC,
    ]);
    let mut input = ReactiveInput {
        steps,
        queued: VecDeque::new(),
        seen: Arc::clone(&seen),
        deadline: Instant::now() + Duration::from_secs(30),
        settle_left: 0,
    };
    let mut port = ObservingPort {
        inner: handle,
        seen: Arc::clone(&seen),
        sent: Arc::clone(&sent),
    };
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    event_loop(&mut terminal, &mut app, &mut port, &mut input).expect("the loop runs to quit");
    assert!(app.should_quit(), "Ctrl-C twice quits");
    assert!(
        port.shutdown(Duration::from_secs(5)),
        "the agent runtime stops within the timeout"
    );

    let screen = text(terminal.backend().buffer());
    // 1. Streamed reply.
    assert!(screen.contains("▎ hello"), "{screen}");
    assert!(
        screen.contains("Hello! I can read and edit files in this workspace."),
        "{screen}"
    );
    // 2. Approval -> deny -> the card shows the denial and the model's follow-up is visible.
    assert!(screen.contains("▸ write_file("), "{screen}");
    assert!(screen.contains("✗ denied"), "{screen}");
    assert!(
        screen.contains("Understood, I did not create the file."),
        "{screen}"
    );
    assert!(
        !workspace.join("hello.txt").exists(),
        "a denied write must not touch the disk"
    );
    // 3. Model and effort.
    assert!(
        screen.contains("Model is now sol (effort xhigh)."),
        "{screen}"
    );
    assert!(
        screen.contains(
            "model sol does not accept effort \"none\"; allowed: low, medium, high, xhigh, max"
        ),
        "{screen}"
    );
    let status = screen.lines().last().expect("status row");
    assert!(
        status.contains("sol/xhigh"),
        "an invalid effort leaves the model alone: {status}"
    );

    let commands = sent.lock().expect("lock").clone();
    assert_eq!(
        commands,
        vec![
            UiCommand::SendUser("hello".to_owned()),
            UiCommand::SendUser("please create hello.txt".to_owned()),
            UiCommand::Approve {
                id: 1,
                decision: harness_agent::approval::Decision::Deny
            },
            UiCommand::SetModel {
                alias: "sol".to_owned(),
                effort: Effort::Xhigh
            },
        ],
        "the invalid /effort never reached the agent"
    );
    assert_costs(&ledger);
    assert_transcript(&session_dir);
}

// -------------------------------------------------------------------------------------------
// Variant B: step by step, with the screen checked after every observation
// -------------------------------------------------------------------------------------------

struct Stepper {
    app: App,
    port: harness_tui::AgentHandle,
    terminal: Terminal<TestBackend>,
}

impl Stepper {
    fn screen(&self) -> String {
        text(self.terminal.backend().buffer())
    }

    fn send_key(&mut self, code: KeyCode) {
        let key = Msg::Key(KeyEvent::new(code, KeyModifiers::NONE));
        drive(&mut self.app, &mut self.port, key);
    }

    fn type_line(&mut self, line: &str) {
        for c in line.chars() {
            self.send_key(KeyCode::Char(c));
        }
        self.send_key(KeyCode::Enter);
    }

    /// Run the loop body (drain events, tick, draw) until `done` holds for the screen.
    fn pump_until(&mut self, what: &str, done: impl Fn(&App, &str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for event in self.port.poll() {
                drive(&mut self.app, &mut self.port, Msg::Ui(event));
            }
            drive(&mut self.app, &mut self.port, Msg::Tick);
            let app = &self.app;
            self.terminal
                .draw(|frame| ui::render(frame, app))
                .expect("draw");
            let screen = self.screen();
            if done(&self.app, &screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn the_scenario_step_by_step_with_screen_assertions() {
    let f = spawn(SetupOpts::new(hello_script()));
    let app = f.app();
    let Fixture {
        handle,
        workspace,
        session_dir,
        ledger,
        mock,
        ..
    } = f;
    let mut s = Stepper {
        app,
        port: handle,
        terminal: Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal"),
    };

    // Opening screen.
    s.pump_until("the opening screen", |_, screen| {
        screen.contains("coding agent · luna")
    });
    assert!(s.screen().contains("luna · effort max"));

    // Observation 1: "hello" gets a streamed reply.
    s.type_line("hello");
    s.pump_until("the reply", |app, screen| {
        !app.busy && screen.contains("Hello! I can read and edit files in this workspace.")
    });
    // The status numbers roll up over a few frames.
    s.pump_until("the status numbers to settle", |_, screen| {
        screen
            .lines()
            .last()
            .is_some_and(|l| l.contains("tokens in 120 out 24"))
    });
    let screen = s.screen();
    assert!(screen.contains("▎ hello"), "{screen}");
    assert!(
        !screen.contains('█'),
        "the cursor block is gone after the reply:\n{screen}"
    );
    assert!(
        screen
            .lines()
            .last()
            .expect("status")
            .contains("tokens in 120 out 24"),
        "{screen}"
    );

    // Observation 2: a tool call needs approval; the diff is coloured; `n` denies.
    s.type_line("please create hello.txt");
    s.pump_until("the approval modal", |_, screen| {
        screen.contains("Approval required")
    });
    let buffer = s.terminal.backend().buffer().clone();
    let screen = text(&buffer);
    assert!(screen.contains("Tool:    write_file"), "{screen}");
    assert!(screen.contains("Class:   write"), "{screen}");
    assert!(screen.contains("Create hello.txt"), "{screen}");
    let (x, y) = find(&buffer, "+hello from the agent").expect("the added line is shown");
    assert_eq!(
        buffer[(x, y)].bg,
        s.app.theme.resolve(s.app.theme.diff_add_bg),
        "added lines carry the diff-add tint"
    );
    assert!(
        !workspace.join("hello.txt").exists(),
        "nothing is written while the modal is open"
    );
    // Enter does not approve; only an explicit key does.
    s.send_key(KeyCode::Enter);
    s.pump_until("the modal to stay", |_, screen| {
        screen.contains("Approval required")
    });
    assert!(!workspace.join("hello.txt").exists());
    s.send_key(KeyCode::Char('n'));
    s.pump_until("the denial and the follow-up", |app, screen| {
        !app.busy
            && screen.contains("✗ denied")
            && screen.contains("Understood, I did not create the file.")
    });
    let screen = s.screen();
    assert!(
        !screen.contains("Approval required"),
        "the modal closed:\n{screen}"
    );
    assert!(
        !workspace.join("hello.txt").exists(),
        "a denied write never touches the disk"
    );
    // Expanding the card shows the denial text as its output.
    s.send_key(KeyCode::Tab);
    s.pump_until("the expanded card", |_, screen| {
        screen.contains("The user denied this tool call.")
    });

    // Observation 3: /model sol, then /effort none is rejected with the allowed list.
    s.type_line("/model sol");
    s.pump_until("the model switch", |_, screen| {
        screen
            .lines()
            .last()
            .is_some_and(|l| l.contains("sol/xhigh"))
    });
    assert!(
        s.screen().contains("Model is now sol (effort xhigh)."),
        "{}",
        s.screen()
    );
    s.type_line("/effort none");
    s.pump_until("the effort error", |_, screen| {
        screen.contains("does not accept effort")
    });
    let screen = s.screen();
    assert!(
        screen.contains("allowed: low, medium, high, xhigh, max"),
        "{screen}"
    );
    assert!(
        screen.lines().last().expect("status").contains("sol/xhigh"),
        "{screen}"
    );

    // Observation 4: Ctrl-C twice quits.
    let ctrl_c = || Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    drive(&mut s.app, &mut s.port, ctrl_c());
    assert!(!s.app.should_quit(), "one Ctrl-C only arms the quit");
    drive(&mut s.app, &mut s.port, ctrl_c());
    assert!(s.app.should_quit());
    assert!(s.port.shutdown(Duration::from_secs(5)));

    // The session directory holds the transcript; costs match the three scripted calls.
    assert_costs(&ledger);
    assert_transcript(&session_dir);
    assert_eq!(
        mock.call_count(),
        3,
        "exactly the three scripted replies were used"
    );
    let user_entries = s
        .app
        .transcript
        .iter()
        .filter(|e| matches!(e, Entry::User(_)))
        .count();
    assert_eq!(user_entries, 2, "slash commands are not sent as messages");
}
