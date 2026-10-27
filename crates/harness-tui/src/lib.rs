//! Interactive coding-agent terminal UI for the MCP tool-knowledge adaptation harness.
//!
//! The user chats with a model that can call built-in coding tools and MCP tools. Risky calls
//! need approval, the model and reasoning effort can be switched, MCP servers can be connected,
//! and drift between a server's live tools and the Tool KB can be reviewed, adapted and
//! accepted. All agent logic lives in `harness-agent`; this crate is the front end.
//!
//! Layout of the crate:
//!
//! - [`app`]: plain-struct [`App`] state and the `Msg` to `Effect` reducer ([`App::update`]).
//!   The reducer never performs I/O.
//! - [`commands`]: slash command parsing. [`completion`]: slash command suggestions.
//! - [`theme`] and [`anim`]: the colour scheme and the frame-counter animations.
//! - [`input`]: the multi-line input buffer.
//! - [`bridge`]: [`AgentHandle`], the thread and runtime that own the async agent, and the
//!   [`AgentPort`] trait the loop uses ([`ScriptedAgentHandle`] for tests).
//! - [`ui`]: rendering, a pure function of the app state.
//! - [`terminal`]: terminal lifecycle, Crossterm polling loop and [`run`].
//!
//! The TUI never asks for, stores or renders credentials: API keys come from the environment
//! variables named in the model registry, read by the caller that builds the agent.

pub mod anim;
pub mod app;
mod artifacts;
pub mod bridge;
pub mod commands;
pub mod completion;
pub mod input;
pub mod terminal;
pub mod theme;
pub mod ui;

pub use app::{
    App, AppInit, ApprovalView, CTRL_C_WINDOW_TICKS, ConfirmAction, ConfirmView, DriftPhase,
    DriftState, DriftView, Effect, Entry, ModelInfo, Msg, Overlay, PAGE_LINES, PickerItem,
    PickerKind, PickerView, ScrollState, StatusDisplay, StatusLine, ToolCard,
};
pub use bridge::{
    AgentHandle, AgentPort, AgentSetup, AgentUiEvent, BridgeError, CanarySummary, ConfigLauncher,
    ConnectFuture, CostSnapshot, ExposedTool, KbSummary, McpLauncher, McpServerStatus, McpState,
    NoMcp, ProbeLine, ScriptedAgentHandle, ScriptedHandle, UiCommand, UiEvent, split_agent_event,
};
pub use input::InputBuffer;
pub use terminal::{
    CrosstermInput, InputSource, RatatuiRestore, SHUTDOWN_TIMEOUT, TICK, TerminalGuard,
    TerminalRestore, TuiError, drive, event_loop, run, run_session,
};
pub use ui::render;
