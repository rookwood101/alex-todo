//! The eframe application shell (task 170).
//!
//! The app owns the SQLite store, the read model, the canvas, the composer and
//! chat panels, the reminder thread, the tray/hotkey handle and the notifier.
//! One frame is:
//!
//! 1. drain platform events (show/hide/new note/quit) and the agent channel;
//! 2. poll PRAGMA data_version and reload the board when another connection
//!    commits (deferred while a note editor has unsaved keystrokes);
//! 3. draw the toolbar, board, chat and composer;
//! 4. apply every returned Command in one batch and reload.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::{
    AgentEvent, AgentHarness, PiConfig, PiHarness, PiProfile, ensure_mcp_config, find_on_path,
    mcp_command_path,
};
use crate::command::{Actor, Applied, Command};
use crate::config::{Config, Profile};
use crate::history;
use crate::model::Board;
use crate::notify::{Notification, Notifier, NotifyError, ToastNotifier};
use crate::platform::{
    Hotkey, Platform, PlatformConfig, PlatformEvent, PlatformHandle, PlatformStart,
};
use crate::remind::{ReminderEngine, ReminderNotice, ReminderSink};
use crate::store::{Store, StorePaths, now_utc_ms, schema};
use crate::theme;
use crate::ui::board::{BoardContext, BoardOutput, BoardView};
use crate::ui::chat::{self, ChatOutput, ChatState, ConnectionTurnHistory, TurnHistory};
use crate::ui::composer::{self, ComposerContext, ComposerState};
use crossbeam_channel::Receiver;
use eframe::egui::{self, Align, Align2, Color32, Key, Layout, Modifiers, RichText, Ui, vec2};

/// How long an in-app status banner stays visible.
const STATUS_SECONDS: u64 = 5;

/// A short message shown over the board.
struct StatusMessage {
    text: String,
    error: bool,
    at: Instant,
    /// Show an Undo button that undoes the last batch.
    undo: bool,
}

/// Notification sink for the reminder engine.
struct ToastSink {
    notifier: Arc<dyn Notifier>,
}

impl ReminderSink for ToastSink {
    fn notify(&self, notice: ReminderNotice) {
        let mut notification = Notification::new(notice.title, notice.body).with_tag(notice.tag);
        if notice.urgent {
            notification = notification.urgent();
        }
        if let Err(error) = self.notifier.notify(notification) {
            eprintln!("rustboard: notification failed: {error}");
        }
    }
}

/// Notifier used when Windows toast registration is unavailable.
struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, notification: Notification) -> Result<(), NotifyError> {
        eprintln!("rustboard: {}: {}", notification.title, notification.body);
        Ok(())
    }

    fn test(&self) -> Result<(), NotifyError> {
        self.notify(Notification::new("Rustboard", "Notifications are working"))
    }
}

/// Turn history used when the store is not open.
struct NoTurnHistory;

impl TurnHistory for NoTurnHistory {
    fn agent_batches_between(
        &self,
        _from_utc_ms: i64,
        _to_utc_ms: i64,
    ) -> Result<Vec<String>, history::HistoryError> {
        Ok(Vec::new())
    }
}

/// Titles of root containers (and the Inbox), offered to `#` capture tags.
fn container_names(board: &Board) -> Vec<String> {
    let mut names: Vec<String> = board
        .notes
        .iter()
        .filter(|note| note.parent.is_none() && (note.inbox || board.has_children(&note.id)))
        .map(|note| note.text.lines().next().unwrap_or("").trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    names
}

/// The application.
pub struct RustboardApp {
    pub(crate) paths: StorePaths,
    pub(crate) store: Option<Store>,
    pub(crate) store_error: Option<String>,
    pub(crate) config: Config,
    config_warnings: Vec<String>,
    pub(crate) board: Board,
    board_tz: String,
    group_names: Vec<String>,
    pub(crate) data_version: i64,
    last_poll: Instant,
    last_ui: Instant,
    pub(crate) reload_deferred: bool,
    pub(crate) view: BoardView,
    pub(crate) composer: ComposerState,
    pub(crate) chat: ChatState,
    pub(crate) canvas_rect: egui::Rect,
    harness: Option<Arc<dyn AgentHarness>>,
    agent_events: Option<Receiver<AgentEvent>>,
    agent_profile: PiProfile,
    platform: Option<PlatformHandle>,
    reminders: Option<ReminderEngine>,
    notifier: Arc<dyn Notifier>,
    notify_error: Option<String>,
    status: Option<StatusMessage>,
    settings_open: bool,
    settings_hotkey: String,
    settings_tz: String,
    settings_model: String,
    settings_profile: Profile,
    settings_reminder: u64,
    quitting: bool,
    pending_new_note: bool,
    full: bool,
    /// Height of the composer panel last frame; the chat panel sits above it.
    composer_height: f32,
    /// The chat was busy last frame (to refocus the composer when it settles).
    chat_was_busy: bool,
}

impl RustboardApp {
    /// Production constructor. The platform handle is started before the window
    /// exists so a second launch can forward to the first and exit.
    pub fn new(cc: &eframe::CreationContext<'_>, platform: Option<PlatformHandle>) -> Self {
        theme::apply(&cc.egui_ctx);
        let paths = StorePaths::resolve(None)
            .unwrap_or_else(|_| StorePaths::from_home(std::env::temp_dir().join("Rustboard")));
        Self::build(paths, platform, true)
    }

    fn build(paths: StorePaths, platform: Option<PlatformHandle>, full: bool) -> Self {
        let (config, warnings) = Config::load_for(&paths);
        let board_tz = config.zone();
        let mut app = Self {
            paths,
            store: None,
            store_error: None,
            settings_hotkey: config.hotkey.clone(),
            settings_tz: config.tz.clone().unwrap_or_default(),
            settings_model: config.pi_model.clone(),
            settings_profile: config.pi_profile,
            settings_reminder: config.reminder_minutes,
            config_warnings: warnings,
            config,
            board: Board::default(),
            board_tz,
            group_names: Vec::new(),
            data_version: -1,
            last_poll: Instant::now(),
            last_ui: Instant::now() - Duration::from_secs(60),
            reload_deferred: false,
            view: BoardView::new(),
            composer: ComposerState::new(),
            chat: ChatState::new(),
            canvas_rect: egui::Rect::ZERO,
            harness: None,
            agent_events: None,
            agent_profile: PiProfile::Board,
            platform,
            reminders: None,
            notifier: Arc::new(LogNotifier),
            notify_error: None,
            status: None,
            settings_open: false,
            quitting: false,
            pending_new_note: false,
            full,
            composer_height: chat::COMPOSER_CLEARANCE,
            chat_was_busy: false,
        };
        app.composer.set_help_visible(app.config.show_syntax_help);
        if full {
            app.start_notifications();
        }
        app.open_store();
        if full && app.store.is_some() {
            app.start_agent_in_background();
        }
        if !app.config_warnings.is_empty() {
            let warning = app.config_warnings.join("; ");
            app.set_status(warning, true);
        }
        app
    }

    /// Test constructor: a private board home, no platform and no OS toasts.
    #[cfg(test)]
    pub(crate) fn for_test(paths: StorePaths, _context: egui::Context) -> Self {
        Self::build(paths, None, false)
    }

    fn start_notifications(&mut self) {
        if let Err(error) = crate::notify::ensure_app_id() {
            self.notify_error = Some(format!("Windows app id: {error}"));
        }
        match ToastNotifier::new() {
            Ok(notifier) => self.notifier = Arc::new(notifier),
            Err(error) => {
                self.notify_error = Some(format!("Windows toasts are unavailable: {error}"));
            }
        }
    }

    fn open_store(&mut self) {
        if let Err(error) = self.paths.ensure_dirs() {
            self.store_error = Some(format!("Cannot create the board folder: {error}"));
            return;
        }
        match schema::open_board(&self.paths) {
            Ok(store) => {
                self.store = Some(store);
                self.store_error = None;
                self.reload_now();
                if self.full {
                    self.start_reminders();
                }
            }
            Err(error) => {
                self.store = None;
                self.store_error = Some(format!("{error}"));
            }
        }
    }

    fn start_reminders(&mut self) {
        self.reminders = None;
        if self.store.is_none() {
            return;
        }
        let sink: Arc<dyn ReminderSink> = Arc::new(ToastSink {
            notifier: Arc::clone(&self.notifier),
        });
        let interval = Duration::from_secs(self.config.reminder_minutes.max(1) * 60);
        match ReminderEngine::start(self.paths.clone(), sink, interval) {
            Ok(engine) => self.reminders = Some(engine),
            Err(error) => self.set_status(format!("Reminders are off: {error}"), true),
        }
    }

    /// Reload the active board from the store.
    pub(crate) fn reload_now(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match Board::load(store.connection()) {
            Ok(board) => {
                self.group_names = container_names(&board);
                self.board = board;
                self.view.after_reload(&self.board);
                self.data_version = store.data_version().unwrap_or(self.data_version);
                self.reload_deferred = false;
            }
            Err(error) => self.set_status(format!("Cannot read the board: {error}"), true),
        }
    }

    /// Apply one batch and reload. Returns the batch result for callers that
    /// need the new ids (the toolbar's New note focuses the new card).
    pub(crate) fn apply_commands(
        &mut self,
        commands: Vec<Command>,
        label: &str,
    ) -> Option<Applied> {
        if commands.is_empty() {
            return None;
        }
        let Some(store) = self.store.as_mut() else {
            self.set_status("The board is not open", true);
            return None;
        };
        match store.apply(&commands, Actor::Human, Some(label)) {
            Ok(applied) => {
                self.reload_now();
                let changed = applied
                    .effects
                    .iter()
                    .map(|effect| effect.affected)
                    .sum::<usize>();
                self.set_status(format!("Saved {changed} change(s)"), false);
                Some(applied)
            }
            Err(error) => {
                self.set_status(format!("{error}"), true);
                None
            }
        }
    }

    /// Force a data_version poll (used by tests and the hidden-window path).
    #[cfg(test)]
    pub(crate) fn poll_for_test(&mut self) {
        self.poll_store(&egui::Context::default(), true);
    }

    pub(crate) fn undo(&mut self) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match history::undo_last(store) {
            Ok(outcome) => {
                self.reload_now();
                self.set_status(format!("Undid {} change(s)", outcome.events), false);
            }
            Err(history::HistoryError::NothingToUndo) => {
                self.set_status("Nothing to undo", false);
            }
            Err(error) => self.set_status(format!("Undo failed: {error}"), true),
        }
    }

    pub(crate) fn redo(&mut self) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match history::redo_last(store) {
            Ok(outcome) => {
                self.reload_now();
                self.set_status(format!("Redid {} change(s)", outcome.events), false);
            }
            Err(history::HistoryError::NothingToRedo) => {
                self.set_status("Nothing to redo", false);
            }
            Err(error) => self.set_status(format!("Redo failed: {error}"), true),
        }
    }

    fn new_note(&mut self) {
        let applied = self.apply_commands(
            vec![Command::AddNote {
                text: String::new(),
                parent: None,
                container: None,
                at_root: false,
                due: None,
                remind: None,
                important: false,
                source: "human".to_owned(),
            }],
            "new note",
        );
        if let Some(id) = applied.and_then(|applied| applied.new_ids.notes.first().cloned()) {
            self.view.edit_note(&self.board, id.clone());
            self.view.reveal(id);
        }
    }

    fn send_prompt(&mut self, prompt: String) {
        let prompt = prompt.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let harness = match self.ensure_harness() {
            Ok(harness) => harness,
            Err(error) => {
                self.chat.error = Some(error.clone());
                self.set_status(error, true);
                return;
            }
        };
        // Record the prompt before sending: a lazy spawn can answer with the
        // resumed history while send() is still running.
        self.chat.push_user_message(prompt.clone());
        chat::begin_turn(&mut self.chat, now_utc_ms());
        if let Err(error) = harness.send(&prompt) {
            // Put the prompt back so nothing typed is lost.
            if self
                .chat
                .messages
                .last()
                .is_some_and(|message| message.text == prompt)
            {
                self.chat.messages.pop();
            }
            self.chat.busy = false;
            self.chat.turn_started_at_utc_ms = None;
            self.chat.error = Some(error.to_string());
            if self.composer.text().trim().is_empty() {
                self.composer.set_text(prompt);
            }
            self.composer.request_focus();
            self.set_status(format!("Pi: {error}"), true);
        }
    }

    /// Start Pi off the UI thread so the resumed conversation appears without
    /// waiting for the first prompt. Missing prerequisites stay silent here;
    /// they are reported when the user sends.
    fn start_agent_in_background(&mut self) {
        if find_on_path("pi").is_none() || !mcp_command_path().is_ok_and(|path| path.is_file()) {
            return;
        }
        let Ok(harness) = self.ensure_harness() else {
            return;
        };
        let profile = self.agent_profile;
        std::thread::spawn(move || {
            if let Err(error) = harness.start(profile) {
                eprintln!("rustboard: pi start failed: {error}");
            }
        });
    }

    /// Why the agent cannot run, in words the user can act on.
    fn agent_prerequisite_error() -> Option<String> {
        if find_on_path("pi").is_none() {
            return Some(
                "Pi is not installed or not on PATH. Install it with \
                 `npm i -g @earendil-works/pi-coding-agent@1.6.0`, then restart Rustboard."
                    .to_owned(),
            );
        }
        match mcp_command_path() {
            Ok(path) if path.is_file() => None,
            Ok(path) => Some(format!(
                "The board tools are missing: {} does not exist. Build all binaries \
                 (`cargo build --bins`) and keep rustboard-mcp next to rustboard.",
                path.display()
            )),
            Err(error) => Some(format!("Cannot locate the board tools: {error}")),
        }
    }

    fn ensure_harness(&mut self) -> Result<Arc<dyn AgentHarness>, String> {
        if let Some(harness) = &self.harness {
            return Ok(Arc::clone(harness));
        }
        if self.full
            && let Some(error) = Self::agent_prerequisite_error()
        {
            return Err(error);
        }
        if let Err(error) = ensure_mcp_config(self.paths.home(), self.paths.db()) {
            return Err(format!("Cannot write the Pi MCP config: {error}"));
        }
        let mut config = PiConfig::new(self.paths.home(), self.agent_profile);
        config.model = self.config.pi_model.clone();
        config.provider = self.config.pi_provider.clone();
        config.session_dir = Some(self.paths.home().join("pi-sessions"));
        config.continue_session = true;
        let harness = PiHarness::new(config);
        if let Err(error) = harness.set_log(&self.paths.logs().join("pi.log")) {
            eprintln!("rustboard: cannot open the Pi log: {error}");
        }
        self.agent_events = Some(harness.events());
        let harness: Arc<dyn AgentHarness> = Arc::new(harness);
        self.harness = Some(Arc::clone(&harness));
        Ok(harness)
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(StatusMessage {
            text: text.into(),
            error,
            at: Instant::now(),
            undo: false,
        });
    }

    /// A status banner with an Undo button for the batch just applied.
    fn set_undoable_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            error: false,
            at: Instant::now(),
            undo: true,
        });
    }

    fn handle_platform(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.platform.as_ref().and_then(PlatformHandle::take_event) {
            match event {
                PlatformEvent::Show => {
                    show_window(ctx);
                    self.composer.request_focus();
                }
                PlatformEvent::NewNote => {
                    show_window(ctx);
                    self.pending_new_note = true;
                }
                PlatformEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                PlatformEvent::Toggle => {
                    if ctx.input(|input| input.viewport().focused.unwrap_or(true)) {
                        self.hide_window(ctx);
                    } else {
                        show_window(ctx);
                    }
                }
            }
        }
        if self.pending_new_note {
            self.pending_new_note = false;
            self.new_note();
        }
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if let Some(platform) = &self.platform
            && let Err(error) = platform.request_hide()
        {
            eprintln!("rustboard: hide failed: {error}");
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.settings_open {
            return;
        }
        let redo_modifiers = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        };
        let (undo, redo, new_note) = ctx.input_mut(|input| {
            (
                input.consume_key(Modifiers::CTRL, Key::Z),
                input.consume_key(redo_modifiers, Key::Z),
                input.consume_key(Modifiers::CTRL, Key::N),
            )
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if new_note {
            self.new_note();
        }
    }

    fn poll_store(&mut self, ctx: &egui::Context, force: bool) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let interval = if self.last_ui.elapsed() < Duration::from_millis(500) {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        if !force {
            let elapsed = self.last_poll.elapsed();
            if elapsed < interval {
                ctx.request_repaint_after(interval - elapsed);
                return;
            }
        }
        self.last_poll = Instant::now();
        match store.data_version() {
            Ok(version) if version != self.data_version => {
                if self.view.is_editing() {
                    self.reload_deferred = true;
                    ctx.request_repaint_after(interval);
                } else {
                    self.reload_now();
                }
            }
            Ok(_) => {}
            Err(error) => self.set_status(format!("Cannot poll the board: {error}"), true),
        }
    }

    fn pump_agent(&mut self) {
        let Some(events) = self.agent_events.as_ref() else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            chat::pump_events(&mut self.chat, events);
            return;
        };
        let history = ConnectionTurnHistory::new(store.connection());
        chat::pump_events_and_end_turn(&mut self.chat, events, &history);
    }

    pub(crate) fn show_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.last_ui = Instant::now();
        let composer_focused = self.composer.has_focus(&ctx);
        self.handle_shortcuts(&ctx);

        egui::Panel::top("toolbar")
            .exact_size(56.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::TOOLBAR_BG)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| self.toolbar(ui));

        let output = self.canvas(ui);
        self.canvas_rect = output.canvas_rect;
        self.apply_board_output(output);

        let chat_output = self.show_chat(&ctx);
        self.handle_chat_output(chat_output);

        self.composer.set_busy(self.chat.busy);
        let composer_context =
            ComposerContext::new(now_utc_ms(), &self.board_tz, &self.group_names);
        if self.chat_was_busy && !self.chat.busy {
            self.composer.request_focus();
        }
        self.chat_was_busy = self.chat.busy;
        let composer_output =
            composer::show(&mut self.composer, &ctx, self.chat.busy, &composer_context);
        if composer_output.panel_height > 0.0 {
            self.composer_height = composer_output.panel_height;
        }
        if let Some(command) = composer_output.capture
            && let Some(applied) = self.apply_commands(vec![command], "capture")
            && let Some(id) = applied.new_ids.notes.first().cloned()
        {
            self.view.reveal(id);
        }
        if let Some(visible) = composer_output.help_toggled {
            self.config.show_syntax_help = visible;
            if let Err(error) = self.config.save(Config::path_for(&self.paths)) {
                self.set_status(format!("Cannot save settings: {error}"), true);
            }
        }
        if let Some(prompt) = composer_output.agent_prompt {
            self.send_prompt(prompt);
        }

        self.show_settings(&ctx);
        self.show_status(&ctx);

        if ctx.input(|input| input.key_pressed(Key::Escape))
            && !composer_focused
            && !ctx.egui_wants_keyboard_input()
            && !self.settings_open
        {
            self.hide_window(&ctx);
        }

        if self.reload_deferred && !self.view.is_editing() {
            self.reload_now();
        }
    }

    fn canvas(&mut self, ui: &mut Ui) -> BoardOutput {
        let now = now_utc_ms();
        let context = BoardContext {
            board: &self.board,
            now_utc_ms: now,
            tz: &self.board_tz,
            hide_done: self.config.hide_done,
        };
        egui::CentralPanel::no_frame()
            .show(ui, |ui| self.view.show(ui, &context))
            .inner
    }

    fn apply_board_output(&mut self, output: BoardOutput) {
        if output.commands.is_empty() {
            return;
        }
        let before = self.board.notes.len();
        let label = if output.deleted_notes > 0 {
            "delete"
        } else {
            "canvas"
        };
        let Some(applied) = self.apply_commands(output.commands, label) else {
            return;
        };
        if output.deleted_notes > 0 {
            let removed = before.saturating_sub(self.board.notes.len());
            let noun = if removed == 1 { "note" } else { "notes" };
            self.set_undoable_status(format!("Deleted {removed} {noun}"));
        }
        if output.edit_new_note
            && let Some(id) = applied.new_ids.notes.first().cloned()
        {
            self.view.edit_note(&self.board, id.clone());
            self.view.reveal(id);
        }
    }

    fn show_chat(&mut self, ctx: &egui::Context) -> ChatOutput {
        match self.store.as_ref() {
            Some(store) => {
                let history = ConnectionTurnHistory::new(store.connection());
                chat::show_above(&mut self.chat, ctx, &history, self.composer_height + 8.0)
            }
            None => chat::show_above(
                &mut self.chat,
                ctx,
                &NoTurnHistory,
                self.composer_height + 8.0,
            ),
        }
    }

    fn handle_chat_output(&mut self, output: ChatOutput) {
        if output.stop_requested
            && let Some(harness) = &self.harness
            && let Err(error) = harness.abort()
        {
            self.set_status(format!("Stop failed: {error}"), true);
        }
        if let Some(ids) = output.undo_turn_requested {
            self.undo_turn(&ids);
        }
        if output.mode_toggled {
            self.agent_profile = self.chat.mode.profile();
            if let Some(harness) = self.harness.clone() {
                // Restarting waits for Pi to exit; keep that off the UI thread.
                // The session resumes, so the conversation continues.
                let profile = self.agent_profile;
                std::thread::spawn(move || {
                    if let Err(error) = harness.start(profile) {
                        eprintln!("rustboard: cannot switch profile: {error}");
                    }
                });
            }
        }
        if output.clear_requested
            && let Some(harness) = self.harness.clone()
        {
            std::thread::spawn(move || {
                if let Err(error) = harness.new_session() {
                    eprintln!("rustboard: cannot start a new session: {error}");
                }
            });
        }
    }

    fn undo_turn(&mut self, ids: &[String]) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match history::undo_batches(store, ids) {
            Ok(outcome) => {
                self.reload_now();
                self.set_status(
                    format!("Undid the agent turn ({} change(s))", outcome.events),
                    false,
                );
            }
            Err(error) => self.set_status(format!("Undo turn failed: {error}"), true),
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.label(
                RichText::new("Rustboard")
                    .size(20.0)
                    .strong()
                    .color(theme::TEXT_PRIMARY),
            );
            ui.add_space(12.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("+  New note")
                            .strong()
                            .color(Color32::from_rgb(0x0e, 0x14, 0x24)),
                    )
                    .fill(theme::ACCENT)
                    .corner_radius(8)
                    .min_size(vec2(112.0, 30.0)),
                )
                .on_hover_text("Add a note in the Inbox (Ctrl+N)")
                .clicked()
            {
                self.new_note();
            }
            if ui.button("Undo").on_hover_text("Undo (Ctrl+Z)").clicked() {
                self.undo();
            }
            if ui
                .button("Redo")
                .on_hover_text("Redo (Ctrl+Shift+Z)")
                .clicked()
            {
                self.redo();
            }
            if ui.button("Fit view").clicked() {
                self.view.fit_next_frame();
            }
            let mut hide_done = self.config.hide_done;
            if ui
                .toggle_value(&mut hide_done, "Hide done")
                .on_hover_text("Hide done notes that have no open children")
                .changed()
            {
                self.set_hide_done(hide_done);
            }
            if ui
                .button("Board folder")
                .on_hover_text(self.paths.home().display().to_string())
                .clicked()
            {
                self.open_board_folder();
            }
            if ui.button("Settings").clicked() {
                self.settings_open = !self.settings_open;
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{:.0}%", self.view.camera().zoom * 100.0))
                        .monospace()
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!(
                        "{} note{}",
                        self.board.notes.len(),
                        if self.board.notes.len() == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(10.0);
                let profile = self.agent_profile_label();
                ui.label(RichText::new(profile).size(12.0).color(theme::TEXT_MUTED))
                    .on_hover_text("Pi tool profile for the chat panel");
            });
        });
    }

    /// Flip the "Hide done" filter and remember it in config.json.
    pub(crate) fn set_hide_done(&mut self, hide: bool) {
        self.config.hide_done = hide;
        if let Err(error) = self.config.save(Config::path_for(&self.paths)) {
            self.set_status(format!("Cannot save settings: {error}"), true);
        }
    }

    fn agent_profile_label(&self) -> &'static str {
        match self.agent_profile {
            PiProfile::Board => "board mode",
            PiProfile::Power => "power mode",
        }
    }

    fn open_board_folder(&mut self) {
        if let Err(error) = self.paths.ensure_dirs() {
            self.set_status(format!("Cannot open the folder: {error}"), true);
            return;
        }
        let result = std::process::Command::new("explorer")
            .arg(self.paths.home())
            .spawn();
        if let Err(error) = result {
            self.set_status(format!("Cannot open the folder: {error}"), true);
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = self.settings_open;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Hotkey");
                        ui.text_edit_singleline(&mut self.settings_hotkey);
                        ui.end_row();

                        ui.label("Board zone");
                        ui.text_edit_singleline(&mut self.settings_tz);
                        ui.end_row();

                        ui.label("Pi model");
                        ui.text_edit_singleline(&mut self.settings_model);
                        ui.end_row();

                        ui.label("Pi profile");
                        egui::ComboBox::from_id_salt("pi_profile")
                            .selected_text(self.settings_profile.as_str())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.settings_profile,
                                    Profile::Board,
                                    "board",
                                );
                                ui.selectable_value(
                                    &mut self.settings_profile,
                                    Profile::Power,
                                    "power",
                                );
                            });
                        ui.end_row();

                        ui.label("Reminder minutes");
                        ui.add(egui::DragValue::new(&mut self.settings_reminder).range(1..=1440));
                        ui.end_row();
                    });
                ui.separator();
                ui.label(
                    RichText::new(format!("Board: {}", self.paths.db().display()))
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
                if let Some(error) = &self.notify_error {
                    ui.label(RichText::new(error).size(11.0).color(theme::SAVE_WARN));
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.save_settings();
                    }
                    if ui.button("Test notification").clicked() {
                        match self.notifier.test() {
                            Ok(()) => self.set_status("Test notification sent", false),
                            Err(error) => {
                                self.set_status(format!("Notification failed: {error}"), true)
                            }
                        }
                    }
                    if ui.button("Undo history").clicked() {
                        self.undo();
                    }
                });
            });
        self.settings_open = open;
    }

    fn save_settings(&mut self) {
        let hotkey = match Hotkey::parse(&self.settings_hotkey) {
            Ok(hotkey) => hotkey,
            Err(error) => {
                self.set_status(format!("Invalid hotkey: {error}"), true);
                return;
            }
        };
        if !self.settings_tz.trim().is_empty()
            && let Err(error) = crate::time::validate_zone(self.settings_tz.trim())
        {
            self.set_status(format!("Invalid zone: {error}"), true);
            return;
        }
        let hotkey_changed = hotkey.to_string() != self.config.hotkey;
        self.config.hotkey = hotkey.to_string();
        self.config.tz =
            (!self.settings_tz.trim().is_empty()).then(|| self.settings_tz.trim().to_owned());
        self.board_tz = self.config.zone();
        self.config.pi_model = self.settings_model.trim().to_owned();
        self.config.pi_profile = self.settings_profile;
        self.config.reminder_minutes = self.settings_reminder.clamp(1, 1440);
        match self.config.save(Config::path_for(&self.paths)) {
            Ok(()) => {
                let note = if hotkey_changed {
                    "Saved; restart to apply the new hotkey"
                } else {
                    "Settings saved"
                };
                self.set_status(note, false);
                if self.full {
                    self.start_reminders();
                }
            }
            Err(error) => self.set_status(format!("Cannot save settings: {error}"), true),
        }
    }

    fn show_status(&mut self, ctx: &egui::Context) {
        let Some(status) = &self.status else {
            return;
        };
        let seconds = if status.undo {
            STATUS_SECONDS * 2
        } else {
            STATUS_SECONDS
        };
        if status.at.elapsed() > Duration::from_secs(seconds) {
            self.status = None;
            return;
        }
        let mut undo_clicked = false;
        egui::Area::new(egui::Id::unique("status_banner"))
            .anchor(Align2::RIGHT_TOP, vec2(-12.0, 66.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::TOOLBAR_BG)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&status.text).size(12.0).color(
                                if status.error {
                                    theme::SAVE_ERROR
                                } else {
                                    theme::SAVE_OK
                                },
                            ));
                            if status.undo
                                && ui
                                    .button(RichText::new("Undo").size(12.0))
                                    .on_hover_text("Ctrl+Z")
                                    .clicked()
                            {
                                undo_clicked = true;
                            }
                        });
                    });
            });
        if undo_clicked {
            self.undo();
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn error_screen(&mut self, ui: &mut Ui) {
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(theme::CANVAS_BG))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.heading("The board could not open");
                    ui.add_space(8.0);
                    if let Some(error) = &self.store_error {
                        ui.label(RichText::new(error).color(theme::SAVE_ERROR));
                    }
                    ui.label(
                        RichText::new(format!("Database: {}", self.paths.db().display()))
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    );
                    ui.add_space(12.0);
                    if ui.button("Retry").clicked() {
                        self.open_store();
                    }
                });
            });
    }
}

impl eframe::App for RustboardApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_platform(ctx);
        self.handle_close(ctx);
        self.pump_agent();
        self.poll_store(ctx, false);
        self.composer.set_busy(self.chat.busy);
        if self.store.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.handle_close(ui.ctx());
        if self.store.is_none() {
            self.error_screen(ui);
            return;
        }
        self.show_ui(ui);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {}

    fn on_exit(&mut self) {
        if let Some(reminders) = &mut self.reminders {
            reminders.stop();
        }
        if let Some(harness) = &self.harness
            && let Err(error) = harness.stop()
        {
            eprintln!("rustboard: stopping Pi failed: {error}");
        }
        if let Some(platform) = &mut self.platform
            && let Err(error) = platform.stop()
        {
            eprintln!("rustboard: stopping the platform failed: {error}");
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::CANVAS_BG.to_normalized_gamma_f32()
    }
}

fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// Platform configuration derived from the saved config.
pub fn platform_config() -> PlatformConfig {
    let paths = StorePaths::resolve(None).ok();
    let hotkey = paths
        .as_ref()
        .and_then(|paths| Hotkey::parse(&Config::load_for(paths).0.hotkey).ok())
        .unwrap_or_default();
    PlatformConfig {
        hotkey,
        ..PlatformConfig::default()
    }
}

/// Start the tray/hotkey thread (called by main before the window exists).
pub fn start_platform(repaint: impl Fn() + Send + 'static) -> (Option<PlatformHandle>, bool) {
    match Platform::start(platform_config(), repaint) {
        Ok(PlatformStart::Started(handle)) => (Some(handle), false),
        Ok(PlatformStart::ForwardedToExisting) => (None, true),
        Err(error) => {
            eprintln!("rustboard: platform integration off: {error}");
            (None, false)
        }
    }
}
