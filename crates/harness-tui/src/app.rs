//! Application state and the `Msg` to `Effect` reducer.
//!
//! [`App::update`] is a pure function of the state and one [`Msg`]: it never performs I/O.
//! Anything that must reach the agent comes back as an [`Effect::Send`]; the terminal loop
//! performs it. Rendering ([`crate::ui::render`]) only reads this state.

use std::collections::{BTreeMap, VecDeque};

use harness_agent::approval::{ApprovalRequest, Decision};
use harness_agent::drift::DriftReport;
use harness_agent::events::StopReason;
use harness_agent::registry::{Effort, ModelSpec};
use harness_core::Strategy;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

use crate::anim;
use crate::bridge::{
    AgentUiEvent, CanarySummary, CostSnapshot, KbSummary, McpServerStatus, McpState, UiCommand,
    UiEvent,
};
use crate::commands::{self, KbCommand, McpCommand, SlashCommand};
use crate::completion::{self, Completion, SuggestState};
use crate::input::InputBuffer;
use crate::terminal::TICK_MS;
use crate::theme::Theme;
use crate::theme::builtin;
use crate::theme::config::ThemeSpec;
use crate::theme::palette::luminance;

/// Ticks in the double Ctrl-C window: one second of wall-clock time at the loop's tick rate.
pub const CTRL_C_WINDOW_TICKS: u32 = (1000 / TICK_MS) as u32;

/// Lines moved by one PgUp or PgDn.
pub const PAGE_LINES: usize = 10;

/// What the UI needs to know about a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Alias used in `/model`.
    pub alias: String,
    /// Provider name.
    pub provider: String,
    /// Efforts the model accepts.
    pub allowed_efforts: Vec<Effort>,
    /// Effort used when switching to the model.
    pub default_effort: Effort,
}

impl ModelInfo {
    /// Describe a registry entry.
    pub fn from_spec(spec: &ModelSpec) -> Self {
        Self {
            alias: spec.alias.clone(),
            provider: spec.provider.as_str().to_owned(),
            allowed_efforts: spec.allowed_efforts.clone(),
            default_effort: spec.default_effort,
        }
    }

    fn efforts_text(&self) -> String {
        self.allowed_efforts
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Initial values for [`App::new`].
#[derive(Debug, Clone)]
pub struct AppInit {
    /// Models offered by `/model`.
    pub models: Vec<ModelInfo>,
    /// Alias of the starting model.
    pub model_alias: String,
    /// Starting effort.
    pub effort: Effort,
    /// Session budget cap in USD.
    pub budget_usd: f64,
    /// Workspace shown in the empty-session banner.
    pub workspace: String,
    /// Configured MCP server names.
    pub mcp_servers: Vec<String>,
}

/// One tool call in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCard {
    /// Provider call id.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as compact JSON.
    pub args: String,
    /// Output, once the call finished.
    pub output: Option<String>,
    /// Whether the call failed.
    pub is_error: bool,
    /// Whether the card shows args and output.
    pub expanded: bool,
    /// Duration, once the call finished.
    pub duration_ms: Option<u64>,
}

/// One item of the visible transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A message the user sent.
    User(String),
    /// Model text. `streaming` is true while deltas are still arriving.
    Assistant {
        /// Text so far.
        text: String,
        /// Whether more text may follow.
        streaming: bool,
    },
    /// A tool call.
    ToolCard(ToolCard),
    /// Informational line.
    Notice(String),
    /// Error line.
    Error(String),
}

/// Contents of the status bar.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLine {
    /// Current model alias.
    pub model_alias: String,
    /// Current effort.
    pub effort: Effort,
    /// Input tokens used this session.
    pub tokens_in: u64,
    /// Output tokens used this session (including reasoning).
    pub tokens_out: u64,
    /// USD spent.
    pub spent_usd: f64,
    /// USD cap.
    pub cap_usd: f64,
    /// Connected MCP servers.
    pub mcp_up: usize,
    /// Configured MCP servers.
    pub mcp_total: usize,
    /// Servers whose live tools differ from the KB.
    pub drift_count: usize,
}

/// Transcript scrolling. `from_bottom == 0` follows the newest line.
///
/// `from_bottom` is the target the keys set; `shown` is the offset that is drawn. Every tick
/// moves `shown` toward `from_bottom` (see [`crate::anim::ease_usize`]), so scrolling glides
/// instead of jumping, and the two are equal once the animation has settled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollState {
    /// Lines between the bottom of the view and the end of the transcript (the target).
    pub from_bottom: usize,
    /// The offset currently drawn; eases toward `from_bottom`.
    pub shown: usize,
}

/// The numbers the status bar draws. They ease toward the real values in
/// [`StatusLine`] so cost and token counts roll up instead of jumping.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StatusDisplay {
    /// Displayed input tokens.
    pub tokens_in: u64,
    /// Displayed output tokens.
    pub tokens_out: u64,
    /// Displayed spend in USD.
    pub spent_usd: f64,
}

/// A pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    /// Request id to answer.
    pub id: u64,
    /// What the agent wants to do.
    pub request: ApprovalRequest,
}

/// Selection state of the drift overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriftView {
    /// Index of the selected server among the drifted servers.
    pub selected: usize,
}

/// Picker value of the row for the theme that came from `theme-file`.
const CUSTOM_THEME_ROW: &str = "custom";

/// One row of a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// Main text.
    pub label: String,
    /// Secondary text.
    pub detail: String,
    /// Value returned on selection.
    pub value: String,
}

/// What a picker chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Pick a model; Enter switches to it.
    Model,
    /// Pick a colour theme; moving the selection previews it, Enter keeps it, Esc restores.
    Theme,
}

/// A list the user picks from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerView {
    /// Title.
    pub title: String,
    /// Rows.
    pub items: Vec<PickerItem>,
    /// Selected row.
    pub selected: usize,
    /// What the picker chooses.
    pub kind: PickerKind,
}

/// What a confirmation does when accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Adopt a server's live tools into the session KB.
    AdoptServer(String),
}

/// A yes/no question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmView {
    /// The question.
    pub prompt: String,
    /// What `y` does.
    pub action: ConfirmAction,
}

/// A modal layer above the chat.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// A tool call waits for approval.
    Approval(ApprovalView),
    /// Drift review.
    Drift(DriftView),
    /// Key and command help.
    Help,
    /// Model picker.
    Picker(PickerView),
    /// Yes/no question.
    Confirm(ConfirmView),
}

/// Where a drifted server is in the adaptation flow.
#[derive(Debug, Clone, PartialEq)]
pub enum DriftPhase {
    /// Waiting for the user to choose a strategy.
    Idle,
    /// A strategy and the canary are running.
    Running(Strategy),
    /// A candidate and its canary result wait for accept or reject.
    Ready(CanarySummary),
}

/// Drift of one server.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftState {
    /// Detected changes.
    pub report: DriftReport,
    /// Adaptation progress.
    pub phase: DriftPhase,
}

/// Input to the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// Bracketed paste.
    Paste(String),
    /// Periodic tick (one animation frame, about 33 ms apart).
    Tick,
    /// An event from the agent runtime.
    Ui(UiEvent),
    /// A failure to show.
    Error(String),
}

/// Something the terminal loop must do for the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send a command to the agent runtime.
    Send(UiCommand),
    /// Leave the application.
    Quit,
}

/// Application state.
#[derive(Debug, Clone)]
pub struct App {
    /// Visible conversation.
    pub transcript: Vec<Entry>,
    /// Message being typed.
    pub input: InputBuffer,
    /// Status bar contents.
    pub status: StatusLine,
    /// Modal layer, when open.
    pub overlay: Option<Overlay>,
    /// A turn is running.
    pub busy: bool,
    /// Transcript scrolling.
    pub scroll: ScrollState,
    /// Models known to `/model`.
    pub models: Vec<ModelInfo>,
    /// MCP servers by name.
    pub mcp: BTreeMap<String, McpServerStatus>,
    /// Session Tool KB summary, once the runtime reported it.
    pub kb: Option<KbSummary>,
    /// Servers whose live tools differ from the KB.
    pub drifts: BTreeMap<String, DriftState>,
    /// Latest budget figures.
    pub cost: CostSnapshot,
    /// Transcript index of the focused tool card.
    pub focus: Option<usize>,
    /// Workspace shown in the empty-session banner.
    pub workspace: String,
    /// Colour scheme. Defaults to the true colour dark theme; the terminal loop replaces it
    /// with one detected from the environment.
    pub theme: Theme,
    /// The theme selection the colours came from (base, overrides). `/theme` and the theme
    /// picker change it for this session only.
    pub theme_spec: ThemeSpec,
    /// Theme selection and colours saved when the theme picker opened, restored on Esc.
    theme_restore: Option<(ThemeSpec, Theme)>,
    /// Frames since start: advanced once per [`Msg::Tick`]. All animations derive from it.
    pub frame: u64,
    /// Frame at which the current overlay opened (for the open animation).
    pub overlay_opened_at: u64,
    /// First visible line of the help overlay.
    pub help_scroll: usize,
    /// Terminal size (width, height) as of the last draw; used to size scrolling.
    pub viewport: (u16, u16),
    /// Status bar numbers as drawn (easing toward `status`).
    pub display: StatusDisplay,
    /// Slash-command suggestion state.
    pub suggest: SuggestState,
    queued_approvals: VecDeque<ApprovalView>,
    quit_armed_ticks: u32,
    quit: bool,
    agent_down: bool,
}

fn compact_args(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

impl App {
    /// Fresh session state.
    pub fn new(init: AppInit) -> Self {
        let mcp: BTreeMap<String, McpServerStatus> = init
            .mcp_servers
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    McpServerStatus {
                        name: name.clone(),
                        state: McpState::Disconnected,
                        live_tools: 0,
                        managed: false,
                        exposed: Vec::new(),
                    },
                )
            })
            .collect();
        let mut app = Self {
            transcript: Vec::new(),
            input: InputBuffer::new(),
            status: StatusLine {
                model_alias: init.model_alias,
                effort: init.effort,
                tokens_in: 0,
                tokens_out: 0,
                spent_usd: 0.0,
                cap_usd: init.budget_usd,
                mcp_up: 0,
                mcp_total: 0,
                drift_count: 0,
            },
            overlay: None,
            busy: false,
            scroll: ScrollState::default(),
            models: init.models,
            mcp,
            kb: None,
            drifts: BTreeMap::new(),
            cost: CostSnapshot {
                spent_usd: 0.0,
                unreconciled_usd: 0.0,
                remaining_usd: init.budget_usd,
                cap_usd: init.budget_usd,
            },
            focus: None,
            workspace: init.workspace,
            theme: Theme::default(),
            theme_spec: ThemeSpec::default(),
            theme_restore: None,
            frame: 0,
            overlay_opened_at: 0,
            help_scroll: 0,
            viewport: (80, 24),
            display: StatusDisplay::default(),
            suggest: SuggestState::default(),
            queued_approvals: VecDeque::new(),
            quit_armed_ticks: 0,
            quit: false,
            agent_down: false,
        };
        app.refresh_status();
        app
    }

    /// Whether the application should exit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether a turn or a drift run is in progress. The agent only accepts changes when idle.
    pub fn blocked(&self) -> bool {
        self.busy
            || self
                .drifts
                .values()
                .any(|d| matches!(d.phase, DriftPhase::Running(_)))
    }

    /// Model info for the current alias.
    pub fn current_model(&self) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|m| m.alias == self.status.model_alias)
    }

    /// Apply one message and return the effects to perform.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let before = self.overlay.as_ref().map(std::mem::discriminant);
        let effects = self.apply(msg);
        self.sync_suggestions();
        let after = self.overlay.as_ref().map(std::mem::discriminant);
        if after != before {
            // A different overlay opened (or one closed): restart the open animation.
            self.overlay_opened_at = self.frame;
            self.help_scroll = 0;
        }
        effects
    }

    fn apply(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Paste(text) => {
                if self.overlay.is_none() {
                    self.input.insert_str(&text);
                }
                Vec::new()
            }
            Msg::Tick => {
                self.quit_armed_ticks = self.quit_armed_ticks.saturating_sub(1);
                self.advance_animations();
                Vec::new()
            }
            Msg::Ui(event) => self.on_ui(event),
            Msg::Error(text) => {
                self.error(text);
                Vec::new()
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------------------------

    /// One animation frame: advance the counter and ease the displayed values.
    fn advance_animations(&mut self) {
        self.frame = self.frame.saturating_add(1);
        self.scroll.shown = anim::ease_usize(self.scroll.shown, self.scroll.from_bottom);
        self.display.tokens_in = anim::ease_u64(self.display.tokens_in, self.status.tokens_in);
        self.display.tokens_out = anim::ease_u64(self.display.tokens_out, self.status.tokens_out);
        self.display.spent_usd =
            anim::ease_f64(self.display.spent_usd, self.status.spent_usd, 0.000_05);
    }

    fn completion_context(&self) -> completion::Context<'_> {
        completion::Context {
            models: &self.models,
            current_model: self.current_model(),
            mcp: &self.mcp,
        }
    }

    /// Slash-command suggestions for the current input, when they should be visible: no
    /// overlay, a single line with the cursor at its end, and not dismissed with Esc.
    pub fn suggestions(&self) -> Option<Completion> {
        if self.overlay.is_some() || !self.input.cursor_at_end() {
            return None;
        }
        let text = self.input.text();
        if self.suggest.dismissed_for.as_deref() == Some(text) {
            return None;
        }
        completion::suggest(text, &self.completion_context())
    }

    /// The selected suggestion row (clamped to the visible suggestions).
    pub fn selected_suggestion(&self, completion: &Completion) -> usize {
        self.suggest
            .selected
            .min(completion.items.len().saturating_sub(1))
    }

    /// A changed input text resets the selection and ends an Esc dismissal.
    fn sync_suggestions(&mut self) {
        if self.suggest.last_text != self.input.text() {
            self.suggest.last_text = self.input.text().to_owned();
            self.suggest.selected = 0;
            if self.suggest.dismissed_for.as_deref() != Some(self.input.text()) {
                self.suggest.dismissed_for = None;
            }
        }
    }

    /// Handle a key while suggestions are visible. `None` means the key is not for the
    /// suggestions and falls through to the normal chat keys.
    fn on_suggest_key(&mut self, key: KeyEvent, completion: &Completion) -> Option<Vec<Effect>> {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let selected = self.selected_suggestion(completion);
        match key.code {
            KeyCode::Up if plain => {
                self.suggest.selected = selected.saturating_sub(1);
            }
            KeyCode::Down if plain => {
                self.suggest.selected = (selected + 1).min(completion.items.len() - 1);
            }
            KeyCode::Tab if plain => self.accept_suggestion(completion, selected),
            KeyCode::Enter if plain => {
                if completion::is_complete(self.input.text(), &self.completion_context()) {
                    return None;
                }
                self.accept_suggestion(completion, selected);
            }
            KeyCode::Esc if plain => {
                self.suggest.dismissed_for = Some(self.input.text().to_owned());
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn accept_suggestion(&mut self, completion: &Completion, index: usize) {
        let text = completion::accept(self.input.text(), completion, index);
        self.input.set_text(&text);
    }

    /// Whether any animation is still moving (the loop may skip redraws when it is not).
    pub fn is_animating(&self) -> bool {
        self.blocked()
            || self.scroll.shown != self.scroll.from_bottom
            || self.display.tokens_in != self.status.tokens_in
            || self.display.tokens_out != self.status.tokens_out
            || self.display.spent_usd != self.status.spent_usd
            || (self.overlay.is_some() && self.frame < self.overlay_opened_at + anim::OPEN_FRAMES)
            || (self.transcript.is_empty()
                && self.frame < anim::BANNER_SHIMMER_START + anim::BANNER_SHIMMER_FRAMES)
            || self
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::ToolCard(c) if c.output.is_none()))
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.transcript.push(Entry::Notice(text.into()));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.transcript.push(Entry::Error(text.into()));
    }

    fn refresh_status(&mut self) {
        self.status.mcp_total = self.mcp.len();
        self.status.mcp_up = self
            .mcp
            .values()
            .filter(|s| s.state == McpState::Connected)
            .count();
        self.status.drift_count = self.drifts.len();
    }

    fn end_stream(&mut self) {
        if let Some(Entry::Assistant { streaming, .. }) = self.transcript.last_mut() {
            *streaming = false;
        }
    }

    fn card_indices(&self) -> Vec<usize> {
        self.transcript
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Entry::ToolCard(_)))
            .map(|(i, _)| i)
            .collect()
    }

    fn refuse_while_blocked(&mut self, what: &str) -> bool {
        if self.blocked() {
            self.notice(format!(
                "{what} is not available while a turn or drift run is in progress."
            ));
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        // The approval modal is a safety control: only y, a, n and Esc act. Nothing else
        // (including Enter) can approve, and there is no default.
        if matches!(self.overlay, Some(Overlay::Approval(_))) {
            return self.on_approval_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.on_ctrl_c();
        }
        match self.overlay {
            Some(Overlay::Help) => {
                self.on_help_key(key);
                Vec::new()
            }
            Some(Overlay::Picker(_)) => self.on_picker_key(key),
            Some(Overlay::Confirm(_)) => self.on_confirm_key(key),
            Some(Overlay::Drift(_)) => self.on_drift_key(key),
            Some(Overlay::Approval(_)) => Vec::new(),
            None => self.on_chat_key(key),
        }
    }

    fn on_ctrl_c(&mut self) -> Vec<Effect> {
        if self.busy {
            self.quit_armed_ticks = 0;
            self.notice("Cancelling the turn...");
            return vec![Effect::Send(UiCommand::Cancel)];
        }
        if self.quit_armed_ticks > 0 {
            self.quit = true;
            return vec![Effect::Quit];
        }
        self.quit_armed_ticks = CTRL_C_WINDOW_TICKS;
        self.notice("Press Ctrl-C again within 1 second to quit.");
        Vec::new()
    }

    fn on_approval_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return Vec::new();
        }
        let decision = match key.code {
            KeyCode::Char('y') => Decision::Once,
            KeyCode::Char('a') => Decision::Session,
            KeyCode::Char('n') | KeyCode::Esc => Decision::Deny,
            _ => return Vec::new(),
        };
        let Some(Overlay::Approval(view)) = self.overlay.take() else {
            return Vec::new();
        };
        self.overlay = self.queued_approvals.pop_front().map(Overlay::Approval);
        vec![Effect::Send(UiCommand::Approve {
            id: view.id,
            decision,
        })]
    }

    fn on_help_key(&mut self, key: KeyEvent) {
        let (width, height) = self.viewport;
        let chat = crate::ui::chat_rect(ratatui::layout::Rect::new(0, 0, width, height), self);
        let max = crate::ui::help::max_scroll(chat);
        let page = crate::ui::help::visible_rows(chat).saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = (self.help_scroll + 1).min(max);
            }
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(page),
            KeyCode::PageDown => self.help_scroll = (self.help_scroll + page).min(max),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = max,
            _ => {}
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(Overlay::Picker(view)) = &mut self.overlay else {
            return Vec::new();
        };
        let kind = view.kind;
        let mut moved = false;
        let mut chosen = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => {
                view.selected = view.selected.saturating_sub(1);
                moved = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                moved = true;
            }
            KeyCode::Enter => {
                chosen = view.items.get(view.selected).map(|i| i.value.clone());
                self.overlay = None;
            }
            _ => {}
        }
        match kind {
            PickerKind::Model => match chosen {
                Some(alias) => self.switch_model(&alias),
                None => Vec::new(),
            },
            PickerKind::Theme => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.restore_theme(),
                    KeyCode::Enter => self.theme_restore = None,
                    _ if moved => {
                        if let Some(value) = self.highlighted_value() {
                            self.preview_theme(&value);
                        }
                    }
                    _ => {}
                }
                Vec::new()
            }
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('y') => {
                let Some(Overlay::Confirm(view)) = self.overlay.take() else {
                    return Vec::new();
                };
                match view.action {
                    ConfirmAction::AdoptServer(server) => {
                        vec![Effect::Send(UiCommand::AdoptServer(server))]
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.overlay = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn selected_drift_server(&self) -> Option<String> {
        let Some(Overlay::Drift(view)) = &self.overlay else {
            return None;
        };
        self.drifts.keys().nth(view.selected).cloned()
    }

    fn on_drift_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let count = self.drifts.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(Overlay::Drift(view)) = &mut self.overlay {
                    view.selected = view.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(Overlay::Drift(view)) = &mut self.overlay {
                    view.selected = (view.selected + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Char('n') => return self.start_drift(Strategy::NoUpdate),
            KeyCode::Char('f') => return self.start_drift(Strategy::FullRewrite),
            KeyCode::Char('s') => return self.start_drift(Strategy::SelectivePatch),
            KeyCode::Char('a') => return self.accept_drift(),
            KeyCode::Char('r') => return self.reject_drift(),
            _ => {}
        }
        Vec::new()
    }

    fn start_drift(&mut self, strategy: Strategy) -> Vec<Effect> {
        let Some(server) = self.selected_drift_server() else {
            return Vec::new();
        };
        if self.refuse_while_blocked("A drift run") {
            return Vec::new();
        }
        if let Some(state) = self.drifts.get_mut(&server) {
            state.phase = DriftPhase::Running(strategy);
        }
        vec![Effect::Send(UiCommand::RunDrift { server, strategy })]
    }

    fn accept_drift(&mut self) -> Vec<Effect> {
        let Some(server) = self.selected_drift_server() else {
            return Vec::new();
        };
        let can_accept = matches!(
            self.drifts.get(&server).map(|d| &d.phase),
            Some(DriftPhase::Ready(summary)) if summary.can_accept
        );
        if !can_accept {
            self.notice(format!(
                "Nothing to accept for {server}: run a strategy and wait for a passing canary."
            ));
            return Vec::new();
        }
        if self.refuse_while_blocked("Accepting a candidate") {
            return Vec::new();
        }
        if let Some(state) = self.drifts.get_mut(&server) {
            state.phase = DriftPhase::Idle;
        }
        vec![Effect::Send(UiCommand::AcceptDrift { server })]
    }

    fn reject_drift(&mut self) -> Vec<Effect> {
        let Some(server) = self.selected_drift_server() else {
            return Vec::new();
        };
        let ready = matches!(
            self.drifts.get(&server).map(|d| &d.phase),
            Some(DriftPhase::Ready(_))
        );
        if !ready {
            return Vec::new();
        }
        if let Some(state) = self.drifts.get_mut(&server) {
            state.phase = DriftPhase::Idle;
        }
        vec![Effect::Send(UiCommand::RejectDrift { server })]
    }

    fn on_chat_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(completion) = self.suggestions()
            && let Some(effects) = self.on_suggest_key(key, &completion)
        {
            return effects;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if alt || shift => self.input.newline(),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('j') if ctrl => self.input.newline(),
            KeyCode::Char('?') if !ctrl && !alt && self.input.is_empty() => {
                self.overlay = Some(Overlay::Help);
            }
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert_char(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::PageUp => {
                self.scroll.from_bottom = self.scroll.from_bottom.saturating_add(PAGE_LINES);
            }
            KeyCode::PageDown => {
                self.scroll.from_bottom = self.scroll.from_bottom.saturating_sub(PAGE_LINES);
            }
            KeyCode::Tab => self.toggle_focused_card(),
            KeyCode::BackTab => self.focus_previous_card(),
            _ => {}
        }
        Vec::new()
    }

    fn toggle_focused_card(&mut self) {
        let target = self.focus.or_else(|| self.card_indices().last().copied());
        if let Some(index) = target
            && let Some(Entry::ToolCard(card)) = self.transcript.get_mut(index)
        {
            card.expanded = !card.expanded;
            self.focus = Some(index);
        }
    }

    fn focus_previous_card(&mut self) {
        let cards = self.card_indices();
        if cards.is_empty() {
            return;
        }
        let current = self
            .focus
            .and_then(|f| cards.iter().position(|&i| i == f))
            .unwrap_or(cards.len() - 1);
        let previous = if current == 0 {
            cards.len() - 1
        } else {
            current - 1
        };
        self.focus = Some(cards[previous]);
    }

    // -----------------------------------------------------------------------------------------
    // Submitting
    // -----------------------------------------------------------------------------------------

    fn submit(&mut self) -> Vec<Effect> {
        let text = self.input.text().trim().to_owned();
        if text.is_empty() {
            return Vec::new();
        }
        match commands::parse(&text) {
            Some(Ok(command)) => {
                self.input.clear();
                self.run_command(command)
            }
            Some(Err(message)) => {
                self.input.clear();
                self.error(message);
                Vec::new()
            }
            None => self.send_message(text),
        }
    }

    fn send_message(&mut self, text: String) -> Vec<Effect> {
        if self.agent_down {
            self.notice("The agent runtime stopped; restart the application.");
            return Vec::new();
        }
        if self.blocked() {
            self.notice(
                "A turn is running. Wait for it to finish or press Ctrl-C to cancel; \
                 your draft is kept.",
            );
            return Vec::new();
        }
        self.input.clear();
        self.transcript.push(Entry::User(text.clone()));
        self.scroll.from_bottom = 0;
        self.busy = true;
        vec![Effect::Send(UiCommand::SendUser(text))]
    }

    /// Make `spec` the session theme. Support and `NO_COLOR` stay as the terminal set them.
    pub fn set_theme_spec(&mut self, spec: ThemeSpec) {
        let resolved = spec.resolve();
        self.theme = Theme {
            support: self.theme.support,
            no_color: self.theme.no_color,
            ..resolved
        };
        self.theme_spec = spec;
    }

    /// Show warnings from loading config files as transcript notices.
    pub fn push_notices(&mut self, notices: Vec<String>) {
        for notice in notices {
            self.notice(notice);
        }
    }

    fn open_theme_picker(&mut self) {
        let custom = self.theme_spec.is_custom();
        let mut items = Vec::new();
        if custom {
            items.push(PickerItem {
                label: "custom".to_owned(),
                detail: self.theme_spec.label(),
                value: CUSTOM_THEME_ROW.to_owned(),
            });
        }
        for entry in builtin::BUILTINS {
            let dark = luminance(builtin::theme(entry).bg) < 0.5;
            items.push(PickerItem {
                label: entry.slug.to_owned(),
                detail: if dark { "dark" } else { "light" }.to_owned(),
                value: entry.slug.to_owned(),
            });
        }
        let wanted = if custom {
            CUSTOM_THEME_ROW.to_owned()
        } else {
            self.theme_spec.label()
        };
        let selected = items.iter().position(|i| i.value == wanted).unwrap_or(0);
        self.theme_restore = Some((self.theme_spec.clone(), self.theme.clone()));
        self.overlay = Some(Overlay::Picker(PickerView {
            title: "Select a theme".to_owned(),
            items,
            selected,
            kind: PickerKind::Theme,
        }));
    }

    /// `/theme <name>`: switch for this session, keeping the config colour overrides.
    fn switch_theme(&mut self, name: &str) {
        match builtin::find(name).and_then(|b| self.theme_spec.with_builtin(b.slug)) {
            Some(spec) => self.set_theme_spec(spec),
            None => self.error(format!(
                "unknown theme {name:?}; valid themes: {}",
                builtin::names().join(", ")
            )),
        }
    }

    /// Preview the picker row `value` against the theme saved when the picker opened.
    fn preview_theme(&mut self, value: &str) {
        let Some((opened, _)) = &self.theme_restore else {
            return;
        };
        let spec = if value == CUSTOM_THEME_ROW {
            Some(opened.clone())
        } else {
            opened.with_builtin(value)
        };
        if let Some(spec) = spec {
            self.set_theme_spec(spec);
        }
    }

    fn restore_theme(&mut self) {
        if let Some((spec, theme)) = self.theme_restore.take() {
            self.theme_spec = spec;
            self.theme = theme;
        }
    }

    fn highlighted_value(&self) -> Option<String> {
        match &self.overlay {
            Some(Overlay::Picker(view)) => view.items.get(view.selected).map(|i| i.value.clone()),
            _ => None,
        }
    }

    fn switch_model(&mut self, alias: &str) -> Vec<Effect> {
        let Some(model) = self.models.iter().find(|m| m.alias == alias).cloned() else {
            let known: Vec<&str> = self.models.iter().map(|m| m.alias.as_str()).collect();
            self.error(format!(
                "unknown model alias {alias:?}; known: {}",
                known.join(", ")
            ));
            return Vec::new();
        };
        vec![Effect::Send(UiCommand::SetModel {
            alias: model.alias,
            effort: model.default_effort,
        })]
    }

    fn run_command(&mut self, command: SlashCommand) -> Vec<Effect> {
        match command {
            SlashCommand::Model(None) => {
                if self.refuse_while_blocked("Switching the model") {
                    return Vec::new();
                }
                let items: Vec<PickerItem> = self
                    .models
                    .iter()
                    .map(|m| PickerItem {
                        label: m.alias.clone(),
                        detail: format!(
                            "{}, efforts: {}, default {}",
                            m.provider,
                            m.efforts_text(),
                            m.default_effort
                        ),
                        value: m.alias.clone(),
                    })
                    .collect();
                let selected = items
                    .iter()
                    .position(|i| i.value == self.status.model_alias)
                    .unwrap_or(0);
                self.overlay = Some(Overlay::Picker(PickerView {
                    title: "Select a model".to_owned(),
                    items,
                    selected,
                    kind: PickerKind::Model,
                }));
                Vec::new()
            }
            SlashCommand::Model(Some(alias)) => {
                if self.refuse_while_blocked("Switching the model") {
                    return Vec::new();
                }
                self.switch_model(&alias)
            }
            SlashCommand::Effort(level) => self.set_effort(&level),
            SlashCommand::Mcp(McpCommand::List) => {
                self.list_mcp();
                Vec::new()
            }
            SlashCommand::Mcp(McpCommand::Connect(name)) => {
                self.mcp_command(&name, "Connecting", UiCommand::ConnectMcp(name.clone()))
            }
            SlashCommand::Mcp(McpCommand::Disconnect(name)) => self.mcp_command(
                &name,
                "Disconnecting",
                UiCommand::DisconnectMcp(name.clone()),
            ),
            SlashCommand::Kb(KbCommand::Show) => {
                self.show_kb();
                Vec::new()
            }
            SlashCommand::Kb(KbCommand::Adopt(server)) => {
                self.adopt(&server);
                Vec::new()
            }
            SlashCommand::Drift => {
                self.overlay = Some(Overlay::Drift(DriftView::default()));
                Vec::new()
            }
            SlashCommand::Cost => {
                self.show_cost();
                Vec::new()
            }
            SlashCommand::Clear => {
                self.transcript.clear();
                self.focus = None;
                self.scroll = ScrollState::default();
                Vec::new()
            }
            SlashCommand::Help => {
                self.overlay = Some(Overlay::Help);
                Vec::new()
            }
            SlashCommand::Theme(None) => {
                self.open_theme_picker();
                Vec::new()
            }
            SlashCommand::Theme(Some(name)) => {
                self.switch_theme(&name);
                Vec::new()
            }
            SlashCommand::Quit => {
                self.quit = true;
                vec![Effect::Quit]
            }
        }
    }

    fn set_effort(&mut self, level: &str) -> Vec<Effect> {
        if self.refuse_while_blocked("Changing the effort") {
            return Vec::new();
        }
        let Some(model) = self.current_model().cloned() else {
            self.error(format!(
                "unknown current model {:?}",
                self.status.model_alias
            ));
            return Vec::new();
        };
        match Effort::parse(&level.to_ascii_lowercase()) {
            Some(effort) if model.allowed_efforts.contains(&effort) => {
                vec![Effect::Send(UiCommand::SetModel {
                    alias: model.alias,
                    effort,
                })]
            }
            _ => {
                self.error(format!(
                    "model {} does not accept effort {level:?}; allowed: {}",
                    model.alias,
                    model.efforts_text()
                ));
                Vec::new()
            }
        }
    }

    fn mcp_command(&mut self, name: &str, verb: &str, command: UiCommand) -> Vec<Effect> {
        if !self.mcp.contains_key(name) {
            let known: Vec<&str> = self.mcp.keys().map(String::as_str).collect();
            let known = if known.is_empty() {
                "none configured".to_owned()
            } else {
                known.join(", ")
            };
            self.error(format!("no MCP server named {name:?}; known: {known}"));
            return Vec::new();
        }
        if self.refuse_while_blocked(verb) {
            return Vec::new();
        }
        vec![Effect::Send(command)]
    }

    fn list_mcp(&mut self) {
        if self.mcp.is_empty() {
            self.notice("No MCP servers configured (start with --mcp-config <path>).");
            return;
        }
        let mut lines = vec!["MCP servers:".to_owned()];
        for status in self.mcp.values() {
            let detail = match &status.state {
                McpState::Disconnected => "disconnected".to_owned(),
                McpState::Connecting => "connecting".to_owned(),
                McpState::Failed(reason) => format!("failed ({reason})"),
                McpState::Connected if status.managed => format!(
                    "connected, {} live tool(s), managed by the KB ({} exposed)",
                    status.live_tools,
                    status.exposed.len()
                ),
                McpState::Connected => format!(
                    "connected, {} live tool(s), not managed by the KB: no tools exposed \
                     (use /kb adopt {})",
                    status.live_tools, status.name
                ),
            };
            lines.push(format!("  {}: {detail}", status.name));
        }
        self.notice(lines.join("\n"));
    }

    fn show_kb(&mut self) {
        let text = match &self.kb {
            Some(kb) if !kb.servers.is_empty() => {
                let mut lines = vec![format!("KB {} (version {}):", kb.kb_id, kb.server_version)];
                for (server, cards) in &kb.servers {
                    lines.push(format!("  {server}: {cards} card(s)"));
                }
                lines.join("\n")
            }
            Some(kb) => format!(
                "KB {} (version {}) has no cards: every MCP server is unmanaged. \
                 Use /kb adopt <server> to snapshot a connected server's live tools.",
                kb.kb_id, kb.server_version
            ),
            None => "No KB information yet.".to_owned(),
        };
        self.notice(text);
    }

    fn adopt(&mut self, server: &str) {
        let Some(status) = self.mcp.get(server) else {
            self.error(format!("no MCP server named {server:?}"));
            return;
        };
        if status.state != McpState::Connected {
            self.error(format!(
                "MCP server {server} is not connected; use /mcp connect {server} first"
            ));
            return;
        }
        if status.managed {
            self.error(format!("server {server} is already managed by the KB"));
            return;
        }
        let prompt = format!(
            "Adopt the {} live tool(s) of {server} into the session KB? \
             They will be exposed to the model as they are listed now.",
            status.live_tools
        );
        if self.refuse_while_blocked("Adopting a server") {
            return;
        }
        self.overlay = Some(Overlay::Confirm(ConfirmView {
            prompt,
            action: ConfirmAction::AdoptServer(server.to_owned()),
        }));
    }

    fn show_cost(&mut self) {
        let c = self.cost;
        self.notice(format!(
            "Spent ${:.4} of ${:.2}; unreconciled ${:.4} (cancelled or unbilled calls count \
             at their worst case); remaining ${:.4}.",
            c.spent_usd, c.cap_usd, c.unreconciled_usd, c.remaining_usd
        ));
    }

    // -----------------------------------------------------------------------------------------
    // Events from the agent runtime
    // -----------------------------------------------------------------------------------------

    fn on_ui(&mut self, event: UiEvent) -> Vec<Effect> {
        match event {
            UiEvent::Agent(event) => self.on_agent(event),
            UiEvent::ApprovalNeeded { id, request } => {
                let view = ApprovalView { id, request };
                if matches!(self.overlay, Some(Overlay::Approval(_))) {
                    self.queued_approvals.push_back(view);
                } else {
                    // An approval takes priority over any other overlay.
                    self.overlay = Some(Overlay::Approval(view));
                }
            }
            UiEvent::ModelChanged { alias, effort } => {
                let changed = self.status.model_alias != alias || self.status.effort != effort;
                self.status.model_alias.clone_from(&alias);
                self.status.effort = effort;
                if changed {
                    self.notice(format!("Model is now {alias} (effort {effort})."));
                }
            }
            UiEvent::Cost(cost) => {
                self.cost = cost;
                self.status.spent_usd = cost.spent_usd;
                self.status.cap_usd = cost.cap_usd;
            }
            UiEvent::McpStatus(status) => {
                if matches!(status.state, McpState::Disconnected | McpState::Failed(_)) {
                    self.drifts.remove(&status.name);
                    self.clamp_drift_selection();
                }
                self.mcp.insert(status.name.clone(), status);
                self.refresh_status();
            }
            UiEvent::KbSummary(kb) => self.kb = Some(kb),
            UiEvent::DriftReady { server, report } => self.set_drift(server, report),
            UiEvent::CanaryReady(summary) => {
                if let Some(state) = self.drifts.get_mut(&summary.server) {
                    state.phase = DriftPhase::Ready(summary);
                }
            }
            UiEvent::DriftFailed { server, message } => {
                if let Some(state) = self.drifts.get_mut(&server) {
                    state.phase = DriftPhase::Idle;
                }
                self.error(format!("drift run for {server} failed: {message}"));
            }
            UiEvent::Notice(text) => self.notice(text),
            UiEvent::Error(text) => self.error(text),
            UiEvent::Fatal(text) => {
                self.busy = false;
                self.agent_down = true;
                self.error(format!("agent runtime stopped: {text}"));
            }
        }
        Vec::new()
    }

    fn clamp_drift_selection(&mut self) {
        let last = self.drifts.len().saturating_sub(1);
        if let Some(Overlay::Drift(view)) = &mut self.overlay {
            view.selected = view.selected.min(last);
        }
    }

    fn set_drift(&mut self, server: String, report: DriftReport) {
        if report.is_empty() {
            self.drifts.remove(&server);
        } else {
            let announce = self
                .drifts
                .get(&server)
                .is_none_or(|d| d.report.diff.changes.len() != report.diff.changes.len());
            if announce {
                self.notice(format!(
                    "Drift on {server}: {} change(s) between the KB and the live tools \
                     (open /drift).",
                    report.diff.changes.len()
                ));
            }
            self.drifts.insert(
                server,
                DriftState {
                    report,
                    phase: DriftPhase::Idle,
                },
            );
        }
        self.clamp_drift_selection();
        self.refresh_status();
    }

    fn on_agent(&mut self, event: AgentUiEvent) {
        match event {
            AgentUiEvent::TurnStarted => self.busy = true,
            AgentUiEvent::TextDelta(delta) => {
                if let Some(Entry::Assistant {
                    text,
                    streaming: true,
                }) = self.transcript.last_mut()
                {
                    text.push_str(&delta);
                } else {
                    self.transcript.push(Entry::Assistant {
                        text: delta,
                        streaming: true,
                    });
                }
            }
            AgentUiEvent::ToolCallStarted {
                call_id,
                name,
                arguments,
            } => {
                self.end_stream();
                self.transcript.push(Entry::ToolCard(ToolCard {
                    call_id,
                    name,
                    args: compact_args(&arguments),
                    output: None,
                    is_error: false,
                    expanded: false,
                    duration_ms: None,
                }));
                self.focus = Some(self.transcript.len() - 1);
            }
            AgentUiEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
                duration_ms,
            } => {
                let found = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match entry {
                        Entry::ToolCard(card) if card.call_id == call_id => Some(card),
                        _ => None,
                    });
                if let Some(card) = found {
                    card.output = Some(output);
                    card.is_error = is_error;
                    card.duration_ms = Some(duration_ms);
                } else {
                    // A call cancelled before it started is reported without a start event.
                    self.end_stream();
                    self.transcript.push(Entry::ToolCard(ToolCard {
                        call_id,
                        name: "tool call".to_owned(),
                        args: String::new(),
                        output: Some(output),
                        is_error,
                        expanded: false,
                        duration_ms: Some(duration_ms),
                    }));
                    self.focus = Some(self.transcript.len() - 1);
                }
            }
            AgentUiEvent::Usage { usage, .. } => {
                self.end_stream();
                self.status.tokens_in += usage.input.unwrap_or(0);
                self.status.tokens_out += usage.output_including_reasoning.unwrap_or(0);
            }
            AgentUiEvent::Drift(report) => {
                let server = report.server.clone();
                self.set_drift(server, report);
            }
            AgentUiEvent::Notice(text) => self.notice(text),
            AgentUiEvent::Error(text) => self.error(text),
            AgentUiEvent::TurnFinished { reason } => {
                self.end_stream();
                self.busy = false;
                // Whatever was pending is moot once the turn is over.
                self.queued_approvals.clear();
                if matches!(self.overlay, Some(Overlay::Approval(_))) {
                    self.overlay = None;
                }
                match reason {
                    StopReason::Completed | StopReason::Error(_) | StopReason::Budget => {}
                    StopReason::Cancelled => self.notice("Turn cancelled."),
                    StopReason::MaxSteps => self.notice("The step limit for one turn was reached."),
                    StopReason::Stalled => self.notice("The model stopped without finishing."),
                }
            }
        }
    }
}
