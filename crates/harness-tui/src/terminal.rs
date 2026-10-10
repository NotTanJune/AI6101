//! Terminal lifecycle and the event loop.
//!
//! Rendering ([`crate::ui::render`]) and input handling are separate: the loop polls Crossterm
//! for input with a timeout, converts what it reads into [`Msg`] values, applies them to the
//! [`App`], performs the resulting [`Effect`]s against the [`AgentPort`] and redraws. Every tick
//! it also drains the agent's events into the app.
//!
//! Terminal restoration: `ratatui::try_init` installs a panic hook that restores the terminal
//! before the previous hook runs (the hook is set before raw mode is enabled, so it also covers
//! a failed init). [`TerminalGuard`] adds restoration on normal exit and on returned errors, and
//! it also runs while a panic unwinds. Restoring twice is harmless. Bracketed paste is enabled
//! after init and disabled on restore, including on panic, so pasted multi-line text reaches
//! the input box as one [`Msg::Paste`].

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use ratatui::crossterm::execute;

use crate::app::{App, Effect, Msg};
use crate::bridge::AgentPort;
use crate::ui;

/// Length of one tick in milliseconds (about 30 frames per second).
pub const TICK_MS: u64 = 33;

/// Tick length: how long the loop waits for input before it polls the agent and advances the
/// animations.
pub const TICK: Duration = Duration::from_millis(TICK_MS);

/// How long quitting waits for the agent runtime to stop.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Failure of the TUI session.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// The terminal could not be put into TUI mode (for example stdout is not a terminal).
    #[error("cannot initialize the terminal: {0}")]
    Init(#[source] io::Error),
    /// Drawing a frame failed.
    #[error("cannot draw the screen: {0}")]
    Draw(String),
    /// Reading terminal input failed.
    #[error("cannot read terminal input: {0}")]
    Input(#[source] io::Error),
}

/// Something that can put the terminal back into its original state.
pub trait TerminalRestore {
    /// Leave raw mode and the alternate screen. Must be safe to call more than once.
    fn restore(&mut self);
}

/// Restores the real terminal through ratatui. Errors are ignored: there is nothing useful to
/// do about them while exiting.
#[derive(Debug, Clone)]
pub struct RatatuiRestore {
    paste_enabled: Arc<AtomicBool>,
}

impl RatatuiRestore {
    /// Restorer that also disables bracketed paste when `paste_enabled` is set.
    pub fn new(paste_enabled: Arc<AtomicBool>) -> Self {
        Self { paste_enabled }
    }
}

impl Default for RatatuiRestore {
    fn default() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)))
    }
}

impl TerminalRestore for RatatuiRestore {
    fn restore(&mut self) {
        if self.paste_enabled.swap(false, Ordering::SeqCst) {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        let _ = ratatui::try_restore();
    }
}

/// Calls [`TerminalRestore::restore`] exactly once when dropped: on normal exit, on a returned
/// error and while a panic unwinds.
#[derive(Debug)]
pub struct TerminalGuard<R: TerminalRestore> {
    restorer: R,
    armed: bool,
}

impl<R: TerminalRestore> TerminalGuard<R> {
    /// Guard that will restore on drop.
    pub fn new(restorer: R) -> Self {
        Self {
            restorer,
            armed: true,
        }
    }
}

impl<R: TerminalRestore> Drop for TerminalGuard<R> {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            self.restorer.restore();
        }
    }
}

/// Source of terminal input events. Separate from rendering so tests can script it.
pub trait InputSource {
    /// Wait up to `timeout` for one event.
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

/// Reads real terminal input with `crossterm::event::poll` and `read`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermInput;

impl InputSource for CrosstermInput {
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

/// Apply `msg`, then perform every resulting effect against `port`.
pub fn drive(app: &mut App, port: &mut dyn AgentPort, msg: Msg) {
    for effect in app.update(msg) {
        match effect {
            Effect::Send(command) => port.send(command),
            // The loop ends when it sees `App::should_quit`.
            Effect::Quit => {}
        }
    }
}

/// Most ticks run in one pass after a stall (the machine slept, a draw was slow); further
/// missed ticks are dropped so animations do not fast-forward.
const MAX_CATCH_UP_TICKS: u32 = 4;

/// Draw, read input, drain agent events, tick; repeat until the app asks to quit.
///
/// Ticks follow the wall clock ([`TICK`] apart), not the number of events, so animations and
/// the double Ctrl-C window keep their speed however fast keys arrive. The screen is redrawn
/// when something happened (input, an agent event) or an animation is running; an idle session
/// does not redraw.
pub fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    port: &mut dyn AgentPort,
    input: &mut dyn InputSource,
) -> Result<(), TuiError> {
    let mut next_tick = Instant::now() + TICK;
    let mut dirty = true;
    loop {
        if dirty {
            if let Ok(size) = terminal.size() {
                app.viewport = (size.width, size.height);
            }
            terminal
                .draw(|frame| ui::render(frame, app))
                .map_err(|e| TuiError::Draw(e.to_string()))?;
            dirty = false;
        }
        if app.should_quit() {
            return Ok(());
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match input.next_event(wait).map_err(TuiError::Input)? {
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                drive(app, port, Msg::Key(key));
                dirty = true;
            }
            Some(Event::Paste(text)) => {
                drive(app, port, Msg::Paste(text));
                dirty = true;
            }
            Some(_) => dirty = true,
            None => {}
        }
        for event in port.poll() {
            drive(app, port, Msg::Ui(event));
            dirty = true;
        }
        let now = Instant::now();
        let mut ticks = 0;
        while now >= next_tick && ticks < MAX_CATCH_UP_TICKS {
            drive(app, port, Msg::Tick);
            next_tick += TICK;
            ticks += 1;
            dirty |= app.is_animating();
        }
        if now >= next_tick {
            next_tick = now + TICK;
        }
    }
}

/// Full session with injected terminal setup, restore and input. [`run`] is this with the real
/// terminal. On every exit path (normal, error) the terminal is restored first and the agent
/// runtime is then asked to stop and given [`SHUTDOWN_TIMEOUT`] to do so.
pub fn run_session<B, R, F>(
    init: F,
    restorer: R,
    input: &mut dyn InputSource,
    app: &mut App,
    port: &mut dyn AgentPort,
) -> Result<(), TuiError>
where
    B: Backend,
    R: TerminalRestore,
    F: FnOnce() -> io::Result<Terminal<B>>,
{
    // Guard first: `init` may fail after enabling raw mode.
    let guard = TerminalGuard::new(restorer);
    let result = match init() {
        Ok(mut terminal) => event_loop(&mut terminal, app, port, input),
        Err(e) => Err(TuiError::Init(e)),
    };
    drop(guard);
    port.shutdown(SHUTDOWN_TIMEOUT);
    result
}

/// Run the TUI on the real terminal until the operator quits.
///
/// The terminal is restored on normal exit, on returned errors and on panics (ratatui's panic
/// hook plus [`TerminalGuard`]). Bracketed paste is on while the TUI runs. When stdin or stdout
/// is not a terminal nothing is touched and [`TuiError::Init`] is returned.
pub fn run(mut app: App, mut port: Box<dyn AgentPort>) -> Result<(), TuiError> {
    // crossterm falls back to /dev/tty when stdin is not a terminal, which would open the TUI
    // on the controlling terminal while the output goes elsewhere. Refuse before touching it.
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        port.shutdown(SHUTDOWN_TIMEOUT);
        return Err(TuiError::Init(io::Error::other(
            "stdin and stdout must both be a terminal",
        )));
    }
    app.theme = app
        .theme
        .clone()
        .with_terminal(&|name| std::env::var(name).ok());
    let paste = Arc::new(AtomicBool::new(false));
    let hook_paste = Arc::clone(&paste);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if hook_paste.swap(false, Ordering::SeqCst) {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        previous(info);
    }));
    let init_paste = Arc::clone(&paste);
    run_session(
        move || {
            let terminal = ratatui::try_init()?;
            execute!(io::stdout(), EnableBracketedPaste)?;
            init_paste.store(true, Ordering::SeqCst);
            Ok(terminal)
        },
        RatatuiRestore::new(paste),
        &mut CrosstermInput,
        &mut app,
        port.as_mut(),
    )
}
