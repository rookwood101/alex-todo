//! Chat transcript panel above the composer (task 130).
//!
//! This module is presentation and turn bookkeeping only: it consumes
//! [AgentEvent]s produced by the Pi bridge (task 120), renders the conversation,
//! and returns [ChatOutput] intents. It never calls the harness, never writes
//! to SQLite, and knows nothing about [crate::app::RustboardApp].
//!
//! Task 170 wiring: pump_events(&mut chat, &harness.events()); then
//! show(&mut chat, ctx, &ConnectionTurnHistory::new(store.connection())) and
//! apply ChatOutput -- stop -> harness.abort(); undo_turn_requested ->
//! history::undo_batches(&mut store, &ids); mode_toggled ->
//! harness.start(chat.mode.profile()).
//!
//! Turn ranges: [begin_turn] records the instant a prompt is sent.
//! [AgentEvent::TurnEnd] finalizes the streamed assistant text and clears busy;
//! the next [show] call (or an explicit [end_turn], or
//! [pump_events_and_end_turn]) asks the [TurnHistory] for the agent apply
//! batches in [turn_started_at_utc_ms, now] and stores them in
//! last_turn_batches. [pump_events] deliberately takes no history, so it stays
//! independent of the database; it leaves the turn range open for the renderer
//! to close. The recorded ids are only an intent: task 170 calls
//! [crate::history::undo_batches], refreshes the board and shows a toast.

use std::collections::HashSet;

use crossbeam_channel::Receiver;
use eframe::egui::{self, Align2, Id, Margin, RichText, Stroke, vec2};
use rusqlite::Connection;

use crate::agent::{AgentEvent, ModelChoice, PiProfile};
use crate::command::Actor;
use crate::history::{self, BatchKind, HistoryError};
use crate::model::BatchId;
use crate::store::now_utc_ms;
use crate::theme;

/// Width of the floating chat panel in points, matching the composer.
const PANEL_WIDTH: f32 = 720.0;

/// Distance between the bottom of the panel and the bottom of the window.
///
/// This leaves room for the one-line composer below it. The app can use the
/// same constant when it positions its own panels.
pub const COMPOSER_CLEARANCE: f32 = 132.0;

/// The transcript may take at most this fraction of the window height.
const MAX_HEIGHT_FRACTION: f32 = 0.40;

/// Header, banner and error rows live outside the scroll area; reserve their
/// height so the whole panel stays under MAX_HEIGHT_FRACTION.
const CHROME_HEIGHT: f32 = 96.0;

/// Never apply more than this many queued events in one frame.
///
/// A flooding child must not stall the UI; live text arrives a frame later
/// instead. The cap is a constant rather than a hidden magic number so tests
/// can assert the exact bound.
const MAX_EVENTS_PER_FRAME: usize = 500;

/// Which tool profile the agent is running under.
///
/// Board is the safe default; Power adds OS tools and therefore gets a warning
/// banner in the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChatMode {
    /// Board tools only.
    #[default]
    Board,
    /// Board tools plus shell and filesystem tools.
    Power,
}

impl ChatMode {
    /// Short label used by the mode badge.
    pub fn label(self) -> &'static str {
        match self {
            Self::Board => "Board",
            Self::Power => "Power",
        }
    }

    /// The other mode, switched by the badge.
    pub fn toggled(self) -> Self {
        match self {
            Self::Board => Self::Power,
            Self::Power => Self::Board,
        }
    }

    /// The Pi profile this mode runs with.
    pub fn profile(self) -> PiProfile {
        match self {
            Self::Board => PiProfile::Board,
            Self::Power => PiProfile::Power,
        }
    }
}

/// Who wrote a [ChatMessage].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    /// A prompt typed by the user.
    User,
    /// Text streamed back by the agent.
    Assistant,
}

impl ChatRole {
    /// Short caption shown above the message text.
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "You",
            Self::Assistant => "Agent",
        }
    }
}

/// One settled transcript message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    /// Who wrote it.
    pub role: ChatRole,
    /// Plain text; markdown is intentionally not rendered.
    pub text: String,
}

/// One compact tool-activity chip.
///
/// ok == None means the tool is still running. Names are stored compacted
/// (board_sql, not mcp__rustboard__board_sql) so the chip stays small.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolChip {
    /// Pi's toolCallId; repeated calls to one tool get their own chips.
    pub id: String,
    /// Compact tool name.
    pub name: String,
    /// Some(true) succeeded, Some(false) failed, None still running.
    pub ok: Option<bool>,
}

impl ToolChip {
    /// Status word used on the chip.
    pub fn status_label(&self) -> &'static str {
        match self.ok {
            Some(true) => "ok",
            Some(false) => "failed",
            None => "running",
        }
    }

    /// Chip text, for example "board_sql | ok".
    pub fn label(&self) -> String {
        format!("{} \u{b7} {}", self.name, self.status_label())
    }
}

/// Token and cost usage reported for the current Pi session.
///
/// Fields are optional because providers do not all report them. [Self::merge]
/// only replaces fields that arrive with a value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostInfo {
    /// Prompt tokens.
    pub input_tokens: Option<u64>,
    /// Completion tokens.
    pub output_tokens: Option<u64>,
    /// Cost in US dollars.
    pub cost_usd: Option<f64>,
}

impl CostInfo {
    /// True when nothing has been reported.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none() && self.output_tokens.is_none() && self.cost_usd.is_none()
    }

    /// Merge newly reported fields without erasing older values.
    pub fn merge(
        &mut self,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    ) {
        self.input_tokens = input_tokens.or(self.input_tokens);
        self.output_tokens = output_tokens.or(self.output_tokens);
        self.cost_usd = cost_usd.or(self.cost_usd);
    }

    /// Human-readable readout, for example "1.2k in | 340 out | USD 0.0042".
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if let Some(input) = self.input_tokens {
            parts.push(format!("{} in", token_count(input)));
        }
        if let Some(output) = self.output_tokens {
            parts.push(format!("{} out", token_count(output)));
        }
        if let Some(cost) = self.cost_usd {
            let text = if cost.abs() < 0.01 {
                format!("{}{cost:.4}", '$')
            } else {
                format!("{}{:.2}", '$', cost)
            };
            parts.push(text);
        }
        parts.join(" \u{b7} ")
    }
}

/// Persistent state of the chat panel.
///
/// Everything is plain data so task 170 can construct, inspect and update it
/// without the widget. The app owns exactly one value.
#[derive(Clone, Debug, Default)]
pub struct ChatState {
    /// Settled user and assistant messages, oldest first.
    pub messages: Vec<ChatMessage>,
    /// Assistant text that has not settled yet (shown live).
    pub streaming: String,
    /// Tool chips for the current or most recent turn.
    pub tool_chips: Vec<ToolChip>,
    /// A turn is being processed.
    pub busy: bool,
    /// Current tool profile.
    pub mode: ChatMode,
    /// Agent apply batches recorded for the most recently finished turn.
    pub last_turn_batches: Vec<BatchId>,
    /// Inline error text (spawn/auth/protocol failures).
    pub error: Option<String>,
    /// Latest token/cost report, when the bridge supplied one.
    pub cost: Option<CostInfo>,
    /// When the current turn's prompt was sent (epoch ms UTC).
    pub turn_started_at_utc_ms: Option<i64>,
    /// Batches known for the current turn before the history range is read.
    ///
    /// [end_turn] merges these with the rows returned by [TurnHistory] and
    /// deduplicates the result into last_turn_batches.
    pub pending_turn_batches: Vec<BatchId>,
    /// Models Pi offers for the configured auth.
    pub models: Vec<ModelChoice>,
    /// The model the session uses, once Pi has reported it.
    pub current_model: Option<ModelChoice>,
    /// Pi's global default (provider, model id) from its settings.json.
    pub default_model: Option<(String, String)>,
}

impl ChatState {
    /// An empty transcript in Board mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a user prompt to the transcript before sending it.
    ///
    /// The chat panel has no input of its own; this is how task 170 records the
    /// prompt that the composer submitted.
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            text: text.into(),
        });
    }

    /// Clear the transcript, live text, chips and inline error.
    ///
    /// Deliberately keeps the mode, token/cost readout, busy flag and
    /// last_turn_batches: Clear must not lose the ability to undo the last turn.
    pub fn clear_transcript(&mut self) {
        self.messages.clear();
        self.streaming.clear();
        self.tool_chips.clear();
        self.error = None;
    }

    /// True when "Undo this turn" may be pressed.
    pub fn can_undo_turn(&self) -> bool {
        !self.busy && !self.last_turn_batches.is_empty()
    }

    /// Move any live text into a settled assistant message.
    fn finalize_streaming(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                text,
            });
        }
    }

    /// Insert or update one tool chip, keyed by call id (or, without an id,
    /// by the newest running chip of that name).
    fn upsert_tool_chip(&mut self, id: &str, name: &str, ok: Option<bool>) {
        let name = compact_tool_name(name).to_owned();
        let existing = if id.is_empty() {
            self.tool_chips
                .iter_mut()
                .rev()
                .find(|chip| chip.name == name && (ok.is_none() || chip.ok.is_none()))
        } else {
            self.tool_chips.iter_mut().find(|chip| chip.id == id)
        };
        match existing {
            Some(chip) if !(id.is_empty() && ok.is_none()) => chip.ok = ok,
            _ => self.tool_chips.push(ToolChip {
                id: id.to_owned(),
                name,
                ok,
            }),
        }
    }

    /// Apply one event. Any order is tolerated; nothing here panics.
    fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Ready => {
                // A successful (re)spawn makes the harness usable again.
                self.error = None;
            }
            AgentEvent::TextDelta(delta) => {
                if !delta.is_empty() {
                    self.streaming.push_str(&delta);
                }
            }
            AgentEvent::TextBreak => {
                if !self.streaming.is_empty() && !self.streaming.ends_with("\n\n") {
                    let trimmed = self.streaming.trim_end_matches('\n').len();
                    self.streaming.truncate(trimmed);
                    self.streaming.push_str("\n\n");
                }
            }
            AgentEvent::ToolStart { id, name } => {
                self.upsert_tool_chip(&id, &name, None);
            }
            AgentEvent::ToolEnd { id, name, ok } => {
                self.upsert_tool_chip(&id, &name, Some(ok));
            }
            AgentEvent::History(history) => {
                // The resumed session is the truth. Keep only a prompt that is
                // already in flight (the lazy spawn case).
                let in_flight = if self.busy {
                    self.messages
                        .last()
                        .filter(|message| message.role == ChatRole::User)
                        .cloned()
                } else {
                    None
                };
                self.messages = history
                    .into_iter()
                    .map(|message| ChatMessage {
                        role: if message.user {
                            ChatRole::User
                        } else {
                            ChatRole::Assistant
                        },
                        text: message.text,
                    })
                    .collect();
                self.messages.extend(in_flight);
            }
            AgentEvent::TurnEnd { .. } => {
                let trimmed = self.streaming.trim_end().len();
                self.streaming.truncate(trimmed);
                self.finalize_streaming();
                self.busy = false;
            }
            AgentEvent::Models(models) => {
                self.models = models;
            }
            AgentEvent::CurrentModel(model) => {
                self.current_model = model;
            }
            AgentEvent::Cost {
                input_tokens,
                output_tokens,
                cost_usd,
            } => {
                let cost = self.cost.get_or_insert_with(CostInfo::default);
                cost.merge(input_tokens, output_tokens, cost_usd);
            }
            AgentEvent::Error(message) => {
                self.error = Some(message);
            }
        }
    }
}

/// Drain the harness channel without blocking and apply up to
/// MAX_EVENTS_PER_FRAME events.
///
/// Returns the number of events applied. A closed or empty receiver is not an
/// error: an empty receiver simply returns zero.
///
/// TurnEnd settles the live assistant text and clears busy, but it leaves
/// turn_started_at_utc_ms set. Call [end_turn] (or [show], which does it
/// automatically) to turn that range into last_turn_batches.
pub fn pump_events(state: &mut ChatState, events: &Receiver<AgentEvent>) -> usize {
    let mut applied = 0;
    while applied < MAX_EVENTS_PER_FRAME {
        match events.try_recv() {
            Ok(event) => {
                state.apply_event(event);
                applied += 1;
            }
            Err(_) => break,
        }
    }
    applied
}

/// Pump events and, if a turn settled during this frame, record its batch range.
///
/// This is the convenience entry point for task 170. [show] performs the same
/// flush, so an app that pumps and draws every frame may use either helper.
pub fn pump_events_and_end_turn(
    state: &mut ChatState,
    events: &Receiver<AgentEvent>,
    history: &dyn TurnHistory,
) -> usize {
    let applied = pump_events(state, events);
    if !state.busy && state.turn_started_at_utc_ms.is_some() {
        end_turn(state, history);
    }
    applied
}

/// Record that a prompt was sent at now_utc_ms.
///
/// Starts a new turn: any text still streaming from an unsettled previous turn
/// is kept as an assistant message, chips are reset, the inline error is
/// cleared, and the previous undo range stays put (the button is disabled while
/// busy).
pub fn begin_turn(state: &mut ChatState, now_utc_ms: i64) {
    state.finalize_streaming();
    state.tool_chips.clear();
    state.pending_turn_batches.clear();
    state.error = None;
    state.turn_started_at_utc_ms = Some(now_utc_ms);
    state.busy = true;
}

/// Finish the current turn: query [start, now] for agent apply batches and
/// store them in [ChatState::last_turn_batches].
///
/// A call with no active turn is harmless and writes nothing. History failures
/// are surfaced through [ChatState::error] and clear the undo range rather than
/// leaving stale ids that would undo the wrong turn.
pub fn end_turn(state: &mut ChatState, history: &dyn TurnHistory) {
    state.busy = false;
    let Some(from_utc_ms) = state.turn_started_at_utc_ms.take() else {
        state.pending_turn_batches.clear();
        return;
    };
    let to_utc_ms = now_utc_ms().max(from_utc_ms);

    let mut batches = std::mem::take(&mut state.pending_turn_batches);
    match history.agent_batches_between(from_utc_ms, to_utc_ms) {
        Ok(found) => batches.extend(found),
        Err(error) => {
            state.error = Some(format!("could not read this turn's history: {error}"));
            state.last_turn_batches.clear();
            return;
        }
    }

    let mut seen = HashSet::new();
    state.last_turn_batches = batches
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect();
}

/// Read-only view of the audit log used by [end_turn].
///
/// Implemented for tests with fixtures and for production by
/// [ConnectionTurnHistory]. It is intentionally not tied to
/// [crate::store::Store], so the chat UI can be tested without a database.
pub trait TurnHistory {
    /// Apply batches with actor = agent and kind = apply created in
    /// [from_utc_ms, to_utc_ms], oldest first.
    fn agent_batches_between(
        &self,
        from_utc_ms: i64,
        to_utc_ms: i64,
    ) -> Result<Vec<BatchId>, HistoryError>;
}

/// Production [TurnHistory] over a live SQLite connection.
///
/// Store::connection() returns exactly the borrow this type needs. The
/// filtering rule -- actor agent, batch kind apply -- lives here so the UI never
/// sees human or system batches, and never sees undo/redo batches.
pub struct ConnectionTurnHistory<'a> {
    conn: &'a Connection,
}

impl<'a> ConnectionTurnHistory<'a> {
    /// Wrap a connection for one or more end_turn calls.
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

impl TurnHistory for ConnectionTurnHistory<'_> {
    fn agent_batches_between(
        &self,
        from_utc_ms: i64,
        to_utc_ms: i64,
    ) -> Result<Vec<BatchId>, HistoryError> {
        let batches = history::batches_between(self.conn, from_utc_ms, to_utc_ms)?;
        Ok(batches
            .into_iter()
            .filter(|batch| batch.actor == Actor::Agent && batch.kind == BatchKind::Apply)
            .map(|batch| batch.id)
            .collect())
    }
}

/// What one chat-panel frame produced.
///
/// At most one control is clicked per frame. The app applies these intents:
/// task 170 calls harness.abort(), history::undo_batches, clears its transcript,
/// or restarts Pi in the new profile, then shows a toast.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatOutput {
    /// The Stop button was pressed; call AgentHarness::abort.
    pub stop_requested: bool,
    /// Undo the recorded batch ids; call history::undo_batches, then refresh.
    pub undo_turn_requested: Option<Vec<BatchId>>,
    /// The Clear button was pressed (the transcript is already cleared here).
    pub clear_requested: bool,
    /// Reserved prompt channel for the composer. The panel itself never sets
    /// this today; task 170 routes composer prompts through
    /// [ChatState::push_user_message] + [begin_turn].
    pub prompt: Option<String>,
    /// The mode badge was pressed; restart the harness with state.mode.profile().
    pub mode_toggled: bool,
    /// A model was picked: (provider, model id) for AgentHarness::set_model.
    pub model_selected: Option<(String, String)>,
}

/// Draw the floating chat panel and report the controls the user pressed.
///
/// The panel is anchored bottom-centre, COMPOSER_CLEARANCE points above the
/// bottom edge, and its transcript scrolls inside at most ~40% of the window
/// height. It never requests keyboard focus, so a long stream cannot steal the
/// composer's focus.
///
/// If a turn settled since the last frame, its batch range is recorded here
/// before the controls are drawn, which is what enables "Undo this turn".
pub fn show(state: &mut ChatState, ctx: &egui::Context, history: &dyn TurnHistory) -> ChatOutput {
    show_above(state, ctx, history, COMPOSER_CLEARANCE)
}

/// [show] with the panel's bottom edge `clearance` points above the window
/// bottom (the app passes the composer's current height). An empty, idle
/// transcript collapses to a one-line bar.
pub fn show_above(
    state: &mut ChatState,
    ctx: &egui::Context,
    history: &dyn TurnHistory,
    clearance: f32,
) -> ChatOutput {
    if !state.busy && state.turn_started_at_utc_ms.is_some() {
        end_turn(state, history);
    }

    let mut output = ChatOutput::default();
    let screen = ctx.content_rect();
    let width = (screen.width() - 24.0).clamp(280.0, PANEL_WIDTH);
    let max_height = (screen.height() * MAX_HEIGHT_FRACTION).max(140.0);
    let scroll_height = (max_height - CHROME_HEIGHT).max(56.0);

    let collapsed = state.messages.is_empty()
        && state.streaming.is_empty()
        && state.tool_chips.is_empty()
        && state.error.is_none()
        && state.last_turn_batches.is_empty()
        && !state.busy;

    egui::Area::new(Id::unique("rustboard.chat"))
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -clearance))
        .order(egui::Order::Foreground)
        .fade_in(false)
        .constrain(true)
        .default_width(width)
        .show(ctx, |ui| {
            ui.set_width(width);
            egui::Frame::new()
                .fill(theme::TOOLBAR_BG)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(12)
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    if collapsed {
                        collapsed_bar(ui, state, &mut output);
                        return;
                    }
                    header(ui, state, &mut output);

                    if state.mode == ChatMode::Power {
                        power_banner(ui);
                    }
                    if let Some(error) = &state.error {
                        error_row(ui, error);
                    }

                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("rustboard.chat.scroll")
                        .max_height(scroll_height)
                        .auto_shrink([false, true])
                        // Streaming follows the tail; a manual scroll unsticks
                        // it until the user returns to the bottom.
                        .stick_to_bottom(true)
                        .show(ui, |ui| transcript(ui, state));
                });
        });

    output
}

/// Header: mode badge, activity hint and cost on one row, actions below.
fn header(ui: &mut egui::Ui, state: &mut ChatState, output: &mut ChatOutput) {
    ui.horizontal(|ui| {
        let mode_button = egui::Button::new(
            RichText::new(format!("{} mode", state.mode.label()))
                .size(11.0)
                .strong()
                .color(theme::TEXT_PRIMARY),
        )
        .fill(theme::ACCENT_SOFT)
        .corner_radius(6)
        .min_size(vec2(88.0, 22.0));
        let mode_response = ui
            .add(mode_button)
            .on_hover_text("Switch between board-only tools (Board) and OS tools (Power)");
        #[cfg(test)]
        test_probe::record("mode", mode_response.rect);
        if mode_response.clicked() {
            state.mode = state.mode.toggled();
            output.mode_toggled = true;
        }

        model_picker(ui, state, output);
        ui.add_space(6.0);
        if state.busy {
            ui.label(
                RichText::new("streaming\u{2026}")
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
        } else if state.error.is_some() {
            ui.label(RichText::new("error").size(11.0).color(theme::SAVE_ERROR));
        } else {
            ui.label(RichText::new("ready").size(11.0).color(theme::TEXT_MUTED));
        }

        if let Some(cost) = state.cost {
            let label = cost.label();
            if !label.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new(label).size(11.0).color(theme::TEXT_MUTED))
                    .on_hover_text("Token usage and cost reported for this Pi session");
            }
        }
    });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let stop = ui.add_enabled(
            state.busy,
            egui::Button::new(RichText::new("Stop").size(11.0)),
        );
        #[cfg(test)]
        test_probe::record("stop", stop.rect);
        if stop.clicked() && state.busy {
            output.stop_requested = true;
        }

        let clear = ui.add(egui::Button::new(RichText::new("Clear").size(11.0)));
        #[cfg(test)]
        test_probe::record("clear", clear.rect);
        if clear.clicked() {
            state.clear_transcript();
            output.clear_requested = true;
        }

        let undo_enabled = state.can_undo_turn();
        let undo = ui.add_enabled(
            undo_enabled,
            egui::Button::new(RichText::new("Undo this turn").size(11.0)),
        );
        #[cfg(test)]
        test_probe::record("undo", undo.rect);
        if undo.clicked() && undo_enabled {
            // Consume the range so a second click cannot undo it twice.
            output.undo_turn_requested = Some(std::mem::take(&mut state.last_turn_batches));
        }
    });
}

/// The model dropdown: lists Pi's models and marks Pi's global default.
fn model_picker(ui: &mut egui::Ui, state: &ChatState, output: &mut ChatOutput) {
    let is_default = |model: &ModelChoice| {
        state
            .default_model
            .as_ref()
            .is_some_and(|(provider, id)| *provider == model.provider && *id == model.id)
    };
    let label = |model: &ModelChoice| {
        if is_default(model) {
            format!("{} (default)", model.name)
        } else {
            model.name.clone()
        }
    };
    let selected = state
        .current_model
        .as_ref()
        .map_or_else(|| "model…".to_owned(), label);
    egui::ComboBox::from_id_salt("rustboard.chat.model")
        .selected_text(RichText::new(selected).size(11.0))
        .width(200.0)
        .show_ui(ui, |ui| {
            if state.models.is_empty() {
                ui.label(
                    RichText::new("Pi has not listed any models yet")
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
            }
            for model in &state.models {
                let current = state.current_model.as_ref() == Some(model);
                if ui
                    .selectable_label(current, label(model))
                    .on_hover_text(format!("{}/{}", model.provider, model.id))
                    .clicked()
                    && !current
                {
                    output.model_selected = Some((model.provider.clone(), model.id.clone()));
                }
            }
        })
        .response
        .on_hover_text("Model for this conversation (Pi keeps your global default)");
}

/// The one-line bar shown while the transcript is empty and idle.
fn collapsed_bar(ui: &mut egui::Ui, state: &mut ChatState, output: &mut ChatOutput) {
    ui.horizontal(|ui| {
        let mode_button = egui::Button::new(
            RichText::new(format!("{} mode", state.mode.label()))
                .size(11.0)
                .strong()
                .color(theme::TEXT_PRIMARY),
        )
        .fill(theme::ACCENT_SOFT)
        .corner_radius(6)
        .min_size(vec2(88.0, 22.0));
        let response = ui
            .add(mode_button)
            .on_hover_text("Switch between board-only tools (Board) and OS tools (Power)");
        #[cfg(test)]
        test_probe::record("mode", response.rect);
        if response.clicked() {
            state.mode = state.mode.toggled();
            output.mode_toggled = true;
        }
        model_picker(ui, state, output);
    });
}

/// Warning strip shown in Power mode.
fn power_banner(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::BUTTON_BG)
        .stroke(Stroke::new(1.0, theme::SAVE_WARN))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.label(
                RichText::new(
                    "Power mode: the agent can run commands and edit files on this machine. Keep prompts specific and only use it when you trust them.",
                )
                .size(11.0)
                .color(theme::SAVE_WARN),
            );
        });
}

/// Inline error strip; shows the tail of the message with the full text on hover.
fn error_row(ui: &mut egui::Ui, error: &str) {
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::BUTTON_BG)
        .stroke(Stroke::new(1.0, theme::SAVE_ERROR))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Error")
                        .size(11.0)
                        .strong()
                        .color(theme::SAVE_ERROR),
                );
                ui.label(
                    RichText::new(tail(error, 400))
                        .size(11.0)
                        .color(theme::SAVE_ERROR),
                )
                .on_hover_text(error);
            });
        });
}

/// The scrolling transcript contents.
fn transcript(ui: &mut egui::Ui, state: &ChatState) {
    let empty =
        state.messages.is_empty() && state.streaming.is_empty() && state.tool_chips.is_empty();
    if empty {
        empty_state(ui, state.mode);
    }

    for message in &state.messages {
        message_block(ui, message.role, &message.text, false);
    }
    if !state.streaming.is_empty() {
        message_block(ui, ChatRole::Assistant, &state.streaming, true);
    }

    if !state.tool_chips.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Tools").size(10.0).color(theme::TEXT_MUTED));
            for chip in &state.tool_chips {
                tool_chip(ui, chip);
            }
        });
    }

    if state.busy && state.streaming.is_empty() && state.tool_chips.is_empty() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(11.0));
            ui.label(
                RichText::new("thinking\u{2026}")
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    }
}

/// Empty-transcript explanation of what the agent can do.
fn empty_state(ui: &mut egui::Ui, mode: ChatMode) {
    ui.label(
        RichText::new("Ask the agent to work on your board.")
            .size(13.0)
            .strong()
            .color(theme::TEXT_PRIMARY),
    );
    let hint = match mode {
        ChatMode::Board => {
            "Board mode: add, move, complete and schedule notes, organise containers, and answer questions about the board."
        }
        ChatMode::Power => {
            "Power mode: board tools plus shell and file access on this machine. Keep prompts specific."
        }
    };
    ui.label(RichText::new(hint).size(11.0).color(theme::TEXT_MUTED));
    ui.label(
        RichText::new("Type below and press Enter; the conversation streams here.")
            .size(11.0)
            .color(theme::TEXT_MUTED),
    );
}

/// One message bubble. User messages are indented to read as the right side.
fn message_block(ui: &mut egui::Ui, role: ChatRole, text: &str, live: bool) {
    let (fill, caption_color) = match role {
        ChatRole::User => (theme::BUTTON_BG, theme::ACCENT),
        ChatRole::Assistant => (theme::HINT_BG, theme::SAVE_OK),
    };
    let indent = if role == ChatRole::User {
        (ui.available_width() * 0.12).clamp(0.0, 64.0)
    } else {
        0.0
    };
    // Compute the wrap width before entering the horizontal layout; inside it
    // the available width is effectively unbounded.
    let content_width = (ui.available_width() - indent - 28.0).max(80.0);

    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.add_space(indent);
        egui::Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(8)
            .inner_margin(Margin::symmetric(9, 6))
            .show(ui, |ui| {
                ui.set_max_width(content_width);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(role.label())
                            .size(10.0)
                            .strong()
                            .color(caption_color),
                    );
                    if live {
                        ui.add(egui::Spinner::new().size(9.0));
                    }
                });
                ui.label(RichText::new(text).size(13.0).color(theme::TEXT_PRIMARY));
            });
    });
}

/// One compact tool chip.
fn tool_chip(ui: &mut egui::Ui, chip: &ToolChip) {
    let color = match chip.ok {
        Some(true) => theme::SAVE_OK,
        Some(false) => theme::SAVE_ERROR,
        None => theme::TEXT_MUTED,
    };
    egui::Frame::new()
        .fill(theme::BUTTON_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(chip.label()).size(10.0).color(color));
        });
}

/// Strip the MCP namespace from a tool name so chips stay compact.
pub fn compact_tool_name(name: &str) -> &str {
    name.strip_prefix("mcp__rustboard__")
        .or_else(|| name.strip_prefix("mcp__"))
        .unwrap_or(name)
}

/// Last max_chars characters of text, prefixed with an ellipsis if truncated.
fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let skip = count - max_chars;
    format!("\u{2026}{}", text.chars().skip(skip).collect::<String>())
}

/// Format a token count compactly.
fn token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Test-only probe that records control rectangles so headless tests can click
/// the real widgets without guessing coordinates.
#[cfg(test)]
mod test_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use eframe::egui::Rect;

    thread_local! {
        static RECTS: RefCell<HashMap<&'static str, Rect>> = RefCell::new(HashMap::new());
    }

    pub fn clear() {
        RECTS.with(|rects| rects.borrow_mut().clear());
    }

    pub fn record(name: &'static str, rect: Rect) {
        RECTS.with(|rects| {
            rects.borrow_mut().insert(name, rect);
        });
    }

    pub fn rect(name: &str) -> Option<Rect> {
        RECTS.with(|rects| rects.borrow().get(name).copied())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crossbeam_channel::unbounded;
    use eframe::egui::{
        Context, Event, Id, Modifiers, PointerButton, Pos2, RawInput, Rect, TextEdit, vec2,
    };

    use super::*;

    const FRAME_SECONDS: f64 = 1.0 / 60.0;
    const SCREEN: [f32; 2] = [1280.0, 800.0];

    /// Fixture history: returns the queued batch lists in order and records the
    /// ranges it was asked about.
    struct FakeHistory {
        batches: RefCell<Vec<Vec<BatchId>>>,
        ranges: RefCell<Vec<(i64, i64)>>,
        fail: bool,
    }

    impl FakeHistory {
        fn new() -> Self {
            Self {
                batches: RefCell::new(Vec::new()),
                ranges: RefCell::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::new()
            }
        }

        fn push_batches(&self, ids: &[&str]) {
            self.batches
                .borrow_mut()
                .push(ids.iter().map(|id| (*id).to_owned()).collect());
        }

        fn last_range(&self) -> Option<(i64, i64)> {
            self.ranges.borrow().last().copied()
        }
    }

    impl TurnHistory for FakeHistory {
        fn agent_batches_between(
            &self,
            from_utc_ms: i64,
            to_utc_ms: i64,
        ) -> Result<Vec<BatchId>, HistoryError> {
            self.ranges.borrow_mut().push((from_utc_ms, to_utc_ms));
            if self.fail {
                return Err(HistoryError::Invalid {
                    message: "fixture failure".to_owned(),
                });
            }
            let mut batches = self.batches.borrow_mut();
            if batches.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(batches.remove(0))
            }
        }
    }

    struct Harness {
        ctx: Context,
        state: ChatState,
        history: FakeHistory,
        sender: crossbeam_channel::Sender<AgentEvent>,
        receiver: crossbeam_channel::Receiver<AgentEvent>,
        screen: Rect,
        time: f64,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_history(FakeHistory::new())
        }

        fn with_history(history: FakeHistory) -> Self {
            test_probe::clear();
            let ctx = Context::default();
            theme::apply(&ctx);
            let (sender, receiver) = unbounded();
            let mut harness = Self {
                ctx,
                state: ChatState::new(),
                history,
                sender,
                receiver,
                screen: Rect::from_min_size(Pos2::ZERO, vec2(SCREEN[0], SCREEN[1])),
                time: 0.0,
            };
            harness.run();
            harness.run();
            harness
        }

        fn send_event(&self, event: AgentEvent) {
            self.sender.send(event).expect("fixture channel open");
        }

        fn pump(&mut self) -> usize {
            pump_events(&mut self.state, &self.receiver)
        }

        fn run(&mut self) -> ChatOutput {
            self.run_events(Vec::new())
        }

        fn run_events(&mut self, events: Vec<Event>) -> ChatOutput {
            self.time += FRAME_SECONDS;
            let raw = RawInput {
                screen_rect: Some(self.screen),
                time: Some(self.time),
                focused: true,
                events,
                ..Default::default()
            };
            let ctx = self.ctx.clone();
            let state = &mut self.state;
            let history = &self.history;
            let mut output = ChatOutput::default();
            let mut frame = ctx.run_ui(raw, |_ui| {
                output = show(state, &ctx, history);
            });
            frame.textures_delta.clear();
            output
        }

        /// Draw a stand-in composer editor before the panel, so focus changes
        /// made by the chat panel are observable.
        fn run_with_editor(&mut self, text: &mut String, id: Id) -> ChatOutput {
            self.time += FRAME_SECONDS;
            let raw = RawInput {
                screen_rect: Some(self.screen),
                time: Some(self.time),
                focused: true,
                events: Vec::new(),
                ..Default::default()
            };
            let ctx = self.ctx.clone();
            let state = &mut self.state;
            let history = &self.history;
            let mut output = ChatOutput::default();
            let mut frame = ctx.run_ui(raw, |ui| {
                ui.add(TextEdit::singleline(text).id(id));
                output = show(state, &ctx, history);
            });
            frame.textures_delta.clear();
            output
        }

        fn click(&mut self, name: &str) -> ChatOutput {
            // Let a freshly shown panel settle (its height can change on the
            // first frame) before reading the control rect we click.
            self.run_events(Vec::new());
            let rect = test_probe::rect(name)
                .unwrap_or_else(|| panic!("control {name:?} was not rendered"));
            let pos = rect.center();
            self.run_events(vec![Event::PointerMoved(pos)]);
            self.run_events(vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }]);
            self.run_events(vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }])
        }

        fn finish_turn(&mut self) {
            end_turn(&mut self.state, &self.history);
        }
    }

    #[test]
    fn text_deltas_settle_into_one_assistant_message() {
        let mut harness = Harness::new();
        harness.history.push_batches(&["turn-1"]);

        harness.state.push_user_message("plan my day");
        begin_turn(&mut harness.state, 1_000);

        harness.send_event(AgentEvent::TextDelta("Here".to_owned()));
        harness.send_event(AgentEvent::TextDelta(" is a plan".to_owned()));
        assert_eq!(harness.pump(), 2);
        assert_eq!(harness.state.streaming, "Here is a plan");
        assert!(harness.state.busy, "streaming keeps the turn busy");
        assert_eq!(
            harness.state.messages.len(),
            1,
            "only the user message settles"
        );
        assert_eq!(harness.state.messages[0].role, ChatRole::User);

        harness.send_event(AgentEvent::Cost {
            input_tokens: Some(1_200),
            output_tokens: Some(340),
            cost_usd: Some(0.0042),
        });
        harness.send_event(AgentEvent::TurnEnd {
            reason: "completed".to_owned(),
        });
        assert_eq!(harness.pump(), 2);

        assert!(!harness.state.busy, "TurnEnd clears busy");
        assert!(harness.state.streaming.is_empty(), "streaming text settled");
        assert_eq!(harness.state.messages.len(), 2);
        assert_eq!(harness.state.messages[1].role, ChatRole::Assistant);
        assert_eq!(harness.state.messages[1].text, "Here is a plan");
        assert_eq!(
            harness.state.cost.and_then(|cost| cost.input_tokens),
            Some(1_200)
        );
        assert!(
            harness.state.last_turn_batches.is_empty(),
            "pump leaves the range open for the renderer"
        );

        harness.run();
        assert_eq!(harness.state.last_turn_batches, vec!["turn-1"]);
        let range = harness.history.last_range().expect("history queried");
        assert_eq!(range.0, 1_000);
        assert!(range.1 >= range.0);
    }

    #[test]
    fn text_blocks_are_separated_and_history_replaces_the_transcript() {
        let mut harness = Harness::new();
        harness.state.push_user_message("go");
        begin_turn(&mut harness.state, 1);
        harness.send_event(AgentEvent::TextBreak);
        harness.send_event(AgentEvent::TextDelta("I'll add that.".to_owned()));
        harness.send_event(AgentEvent::TextBreak);
        harness.send_event(AgentEvent::TextDelta("Done.".to_owned()));
        harness.send_event(AgentEvent::TurnEnd {
            reason: "completed".to_owned(),
        });
        harness.pump();
        assert_eq!(
            harness.state.messages.last().map(|m| m.text.as_str()),
            Some("I'll add that.\n\nDone.")
        );

        harness.send_event(AgentEvent::History(vec![
            crate::agent::HistoryMessage {
                user: true,
                text: "earlier".to_owned(),
            },
            crate::agent::HistoryMessage {
                user: false,
                text: "reply".to_owned(),
            },
        ]));
        harness.pump();
        let texts: Vec<&str> = harness
            .state
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, vec!["earlier", "reply"]);

        harness.state.push_user_message("in flight");
        begin_turn(&mut harness.state, 2);
        harness.send_event(AgentEvent::History(vec![crate::agent::HistoryMessage {
            user: true,
            text: "earlier".to_owned(),
        }]));
        harness.pump();
        let texts: Vec<&str> = harness
            .state
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, vec!["earlier", "in flight"], "the sent prompt stays");
    }

    #[test]
    fn tool_chips_appear_update_and_fail() {
        let mut harness = Harness::new();
        let start = |id: &str, name: &str| AgentEvent::ToolStart {
            id: id.to_owned(),
            name: format!("mcp__rustboard__{name}"),
        };
        let end = |id: &str, name: &str, ok: bool| AgentEvent::ToolEnd {
            id: id.to_owned(),
            name: format!("mcp__rustboard__{name}"),
            ok,
        };

        harness.send_event(start("c1", "board_sql"));
        assert_eq!(harness.pump(), 1);
        assert_eq!(harness.state.tool_chips.len(), 1);
        assert_eq!(harness.state.tool_chips[0].name, "board_sql");
        assert_eq!(harness.state.tool_chips[0].status_label(), "running");
        assert_eq!(
            harness.state.tool_chips[0].label(),
            "board_sql \u{b7} running"
        );

        harness.send_event(end("c1", "board_sql", false));
        assert_eq!(harness.pump(), 1);
        assert_eq!(harness.state.tool_chips.len(), 1, "same id updates");
        assert_eq!(harness.state.tool_chips[0].ok, Some(false));
        assert_eq!(
            harness.state.tool_chips[0].label(),
            "board_sql \u{b7} failed"
        );

        // Two parallel calls to one tool keep separate chips.
        harness.send_event(start("c2", "add_note"));
        harness.send_event(start("c3", "add_note"));
        harness.send_event(end("c2", "add_note", true));
        assert_eq!(harness.pump(), 3);
        assert_eq!(harness.state.tool_chips.len(), 3);
        assert_eq!(harness.state.tool_chips[1].ok, Some(true));
        assert_eq!(harness.state.tool_chips[2].ok, None, "c3 still runs");

        // An unexpected ToolEnd adds a chip instead of panicking.
        harness.send_event(end("zz", "add_note", false));
        assert_eq!(harness.pump(), 1);
        assert_eq!(harness.state.tool_chips.len(), 4);
        assert_eq!(harness.state.tool_chips[3].ok, Some(false));

        harness.run();
    }

    #[test]
    fn compact_names_strip_the_mcp_namespace() {
        assert_eq!(
            compact_tool_name("mcp__rustboard__board_apply"),
            "board_apply"
        );
        assert_eq!(compact_tool_name("mcp__rustboard__board_sql"), "board_sql");
        assert_eq!(compact_tool_name("mcp__other__thing"), "other__thing");
        assert_eq!(compact_tool_name("plain"), "plain");
    }

    #[test]
    fn stop_is_requested_only_while_busy() {
        let mut harness = Harness::new();
        begin_turn(&mut harness.state, 1_000);
        assert!(harness.state.busy);

        harness.run();
        let output = harness.click("stop");
        assert!(output.stop_requested);
        assert!(
            harness.state.busy,
            "the panel waits for the abort to settle"
        );

        harness.state.busy = false;
        harness.run();
        let output = harness.click("stop");
        assert!(
            !output.stop_requested,
            "Stop is disabled when no turn is running"
        );
    }

    #[test]
    fn undo_passes_the_recorded_batch_ids_and_disables_without_a_turn() {
        let mut harness = Harness::new();
        harness.history.push_batches(&["batch-a", "batch-b"]);

        begin_turn(&mut harness.state, 5_000);
        harness
            .state
            .pending_turn_batches
            .push("batch-b".to_owned());
        harness.finish_turn();

        assert_eq!(
            harness.state.last_turn_batches,
            vec!["batch-b", "batch-a"],
            "pending ids come first, duplicates disappear, history follows"
        );
        harness.run();
        let output = harness.click("undo");
        assert_eq!(
            output.undo_turn_requested,
            Some(vec!["batch-b".to_owned(), "batch-a".to_owned()])
        );
        assert!(harness.state.last_turn_batches.is_empty());

        // With no recorded turn the button is disabled.
        harness.run();
        let output = harness.click("undo");
        assert_eq!(output.undo_turn_requested, None);

        // Busy also disables it, even with ids waiting.
        harness.state.last_turn_batches = vec!["stale".to_owned()];
        harness.state.busy = true;
        harness.run();
        let output = harness.click("undo");
        assert_eq!(output.undo_turn_requested, None);
        assert_eq!(harness.state.last_turn_batches, vec!["stale"]);
    }

    #[test]
    fn errors_render_and_ready_clears_them() {
        let mut harness = Harness::new();
        let error = format!(
            "No API key found for deepseek.\n{}ERROR TAIL",
            "x".repeat(1_000)
        );
        harness.send_event(AgentEvent::Error(error.clone()));
        assert_eq!(harness.pump(), 1);
        assert_eq!(harness.state.error.as_deref(), Some(error.as_str()));

        // Rendering a very long error must not panic.
        harness.run();
        harness.run();
        assert!(tail(&error, 20).ends_with("ERROR TAIL"));
        assert!(tail(&error, 20).starts_with('\u{2026}'));

        harness.send_event(AgentEvent::Ready);
        assert_eq!(harness.pump(), 1);
        assert_eq!(harness.state.error, None, "Ready makes the harness usable");
    }

    #[test]
    fn an_empty_turn_is_harmless() {
        let mut harness = Harness::new();
        begin_turn(&mut harness.state, 7_000);
        harness.finish_turn();

        assert!(!harness.state.busy);
        assert!(harness.state.last_turn_batches.is_empty());
        assert!(harness.state.messages.is_empty());
        assert!(harness.state.streaming.is_empty());
        assert_eq!(harness.state.error, None);
        let range = harness.history.last_range().expect("range queried");
        assert_eq!(range.0, 7_000);
        assert!(range.1 >= range.0);
        harness.run();
    }

    #[test]
    fn an_orphan_turn_end_still_settles_the_text() {
        let mut harness = Harness::new();
        harness.send_event(AgentEvent::TextDelta("orphan".to_owned()));
        harness.send_event(AgentEvent::TurnEnd {
            reason: "aborted".to_owned(),
        });
        assert_eq!(harness.pump(), 2);

        assert!(!harness.state.busy);
        assert_eq!(harness.state.messages.len(), 1);
        assert_eq!(harness.state.messages[0].text, "orphan");
        assert_eq!(harness.state.turn_started_at_utc_ms, None);
        harness.run();
        assert!(
            harness.state.last_turn_batches.is_empty(),
            "without begin_turn there is no range to record"
        );
    }

    #[test]
    fn pump_caps_events_per_frame() {
        let mut harness = Harness::new();
        for _ in 0..(MAX_EVENTS_PER_FRAME + 7) {
            harness.send_event(AgentEvent::TextDelta("x".to_owned()));
        }
        assert_eq!(harness.pump(), MAX_EVENTS_PER_FRAME);
        assert_eq!(harness.state.streaming.len(), MAX_EVENTS_PER_FRAME);
        assert_eq!(harness.pump(), 7);
        assert_eq!(harness.state.streaming.len(), MAX_EVENTS_PER_FRAME + 7);

        // An empty receiver, and a disconnected one, are both harmless.
        let (sender, receiver) = unbounded::<AgentEvent>();
        let mut state = ChatState::new();
        assert_eq!(pump_events(&mut state, &receiver), 0);
        drop(sender);
        assert_eq!(pump_events(&mut state, &receiver), 0);
    }

    #[test]
    fn multiple_turns_keep_messages_ordered() {
        let mut harness = Harness::new();
        harness.history.push_batches(&["first-turn"]);
        harness.history.push_batches(&["second-turn"]);

        harness.state.push_user_message("first prompt");
        begin_turn(&mut harness.state, 1_000);
        harness.send_event(AgentEvent::TextDelta("first answer".to_owned()));
        harness.send_event(AgentEvent::TurnEnd {
            reason: "completed".to_owned(),
        });
        harness.pump();
        harness.finish_turn();
        assert_eq!(harness.state.last_turn_batches, vec!["first-turn"]);

        harness.state.push_user_message("second prompt");
        begin_turn(&mut harness.state, 2_000);
        harness.send_event(AgentEvent::TextDelta("second answer".to_owned()));
        harness.send_event(AgentEvent::TurnEnd {
            reason: "completed".to_owned(),
        });
        harness.pump();
        harness.finish_turn();
        assert_eq!(harness.state.last_turn_batches, vec!["second-turn"]);

        let roles: Vec<ChatRole> = harness.state.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::User,
                ChatRole::Assistant
            ]
        );
        let texts: Vec<&str> = harness
            .state
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "first prompt",
                "first answer",
                "second prompt",
                "second answer"
            ]
        );
    }

    #[test]
    fn clear_empties_the_transcript_only() {
        let mut harness = Harness::new();
        harness.state.push_user_message("keep the undo range");
        harness.state.streaming = "live".to_owned();
        harness.state.tool_chips.push(ToolChip {
            id: "c1".to_owned(),
            name: "board_sql".to_owned(),
            ok: Some(true),
        });
        harness.state.error = Some("boom".to_owned());
        harness.state.last_turn_batches = vec!["batch-a".to_owned()];
        harness.state.cost = Some(CostInfo {
            input_tokens: Some(10),
            output_tokens: None,
            cost_usd: Some(0.001),
        });
        harness.state.mode = ChatMode::Power;
        harness.state.busy = true;

        harness.state.clear_transcript();

        assert!(harness.state.messages.is_empty());
        assert!(harness.state.streaming.is_empty());
        assert!(harness.state.tool_chips.is_empty());
        assert_eq!(harness.state.error, None);
        assert_eq!(harness.state.last_turn_batches, vec!["batch-a"]);
        assert!(harness.state.cost.is_some());
        assert_eq!(harness.state.mode, ChatMode::Power);
        assert!(harness.state.busy);

        // The button path clears the transcript and reports the intent but
        // keeps the undo range, so a turn can still be undone after Clear.
        let mut harness = Harness::new();
        harness.state.push_user_message("visible");
        harness.state.last_turn_batches = vec!["batch-a".to_owned()];
        harness.run();
        let output = harness.click("clear");
        assert!(output.clear_requested);
        assert!(harness.state.messages.is_empty());
        assert_eq!(harness.state.last_turn_batches, vec!["batch-a"]);
    }

    #[test]
    fn mode_badge_toggles_and_reports() {
        let mut harness = Harness::new();
        assert_eq!(harness.state.mode, ChatMode::Board);
        harness.run();
        let output = harness.click("mode");
        assert!(output.mode_toggled);
        assert_eq!(harness.state.mode, ChatMode::Power);
        assert_eq!(harness.state.mode.profile(), PiProfile::Power);

        // The Power warning banner renders without panicking.
        harness.run();

        let output = harness.click("mode");
        assert!(output.mode_toggled);
        assert_eq!(harness.state.mode, ChatMode::Board);
        assert_eq!(harness.state.mode.profile(), PiProfile::Board);
    }

    #[test]
    fn cost_readout_formats_and_merges() {
        let mut harness = Harness::new();
        harness.send_event(AgentEvent::Cost {
            input_tokens: Some(1_200),
            output_tokens: Some(340),
            cost_usd: Some(0.0042),
        });
        assert_eq!(harness.pump(), 1);
        let label = harness.state.cost.expect("cost").label();
        assert!(label.contains("1.2k in"), "{label}");
        assert!(label.contains("340 out"), "{label}");
        assert!(label.contains("$0.0042"), "{label}");

        // Missing fields keep the previous value.
        harness.send_event(AgentEvent::Cost {
            input_tokens: None,
            output_tokens: Some(500),
            cost_usd: None,
        });
        assert_eq!(harness.pump(), 1);
        let cost = harness.state.cost.expect("cost");
        assert_eq!(cost.input_tokens, Some(1_200));
        assert_eq!(cost.output_tokens, Some(500));
        assert_eq!(cost.cost_usd, Some(0.0042));

        harness.run();
    }

    #[test]
    fn history_failure_is_inline_and_does_not_leave_stale_undo_ids() {
        let mut harness = Harness::with_history(FakeHistory::failing());
        harness.state.last_turn_batches = vec!["stale".to_owned()];
        begin_turn(&mut harness.state, 1_000);
        harness.finish_turn();

        assert!(!harness.state.busy);
        assert!(harness.state.last_turn_batches.is_empty());
        let error = harness.state.error.as_deref().expect("inline error");
        assert!(error.contains("fixture failure"), "{error}");
        harness.run();
    }

    #[test]
    fn long_stream_scrolls_without_stealing_focus_and_stays_capped() {
        let mut harness = Harness::new();
        let editor = Id::unique("chat.test.composer");
        let mut text = String::new();

        // A stand-in composer editor owns keyboard focus.
        harness.run_with_editor(&mut text, editor);
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(editor));
        harness.run_with_editor(&mut text, editor);
        assert!(harness.ctx.memory(|memory| memory.has_focus(editor)));

        harness.state.push_user_message("write a long answer");
        begin_turn(&mut harness.state, 1_000);
        for _ in 0..200 {
            harness.send_event(AgentEvent::TextDelta("word ".to_owned()));
        }
        assert_eq!(harness.pump(), 200);
        harness.run_with_editor(&mut text, editor);
        // egui anchors an area with its previous size; the panel just grew
        // from the collapsed bar, so let it settle for one frame.
        harness.run_with_editor(&mut text, editor);

        assert!(
            harness.ctx.memory(|memory| memory.has_focus(editor)),
            "the streaming panel must not steal composer focus"
        );

        let area = harness
            .ctx
            .memory(|memory| memory.area_rect(Id::unique("rustboard.chat")))
            .expect("chat area is laid out");
        assert!(
            area.height() <= SCREEN[1] * MAX_HEIGHT_FRACTION + 16.0,
            "panel should stay near 40% of the window, got {}",
            area.height()
        );
        assert!(
            area.bottom() <= SCREEN[1] - COMPOSER_CLEARANCE + 32.0,
            "panel should sit above the composer, bottom={}",
            area.bottom()
        );
        assert!(
            area.top() >= 0.0,
            "constrained panel should stay on screen, top={}",
            area.top()
        );
    }

    #[test]
    fn empty_state_explains_what_the_agent_can_do() {
        let mut harness = Harness::new();
        harness.run();
        let area = harness
            .ctx
            .memory(|memory| memory.area_rect(Id::unique("rustboard.chat")))
            .expect("chat area is laid out");
        assert!(area.height() > 0.0);
        // Rendering both modes exercises both empty-state hints.
        harness.state.mode = ChatMode::Power;
        harness.run();
    }

    #[test]
    fn late_events_after_turn_end_do_not_panic() {
        let mut state = ChatState::new();
        let (sender, receiver) = unbounded();
        sender
            .send(AgentEvent::TurnEnd {
                reason: "completed".to_owned(),
            })
            .expect("send");
        sender
            .send(AgentEvent::TextDelta("late".to_owned()))
            .expect("send");
        sender
            .send(AgentEvent::ToolEnd {
                id: "c1".to_owned(),
                name: "board_sql".to_owned(),
                ok: true,
            })
            .expect("send");
        assert_eq!(pump_events(&mut state, &receiver), 3);
        assert!(!state.busy);
        assert_eq!(state.streaming, "late");
        assert_eq!(state.tool_chips.len(), 1);
        assert!(state.messages.is_empty());
        assert_eq!(pump_events(&mut state, &receiver), 0);
    }
}
