use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, Key, Layout, Modifiers, PointerButton, Rect, RichText, Sense, Ui, Vec2,
    vec2,
};
use serde::{Deserialize, Serialize};

use crate::board::{Board, NoteId};
use crate::camera::Camera;
use crate::canvas;
use crate::persist::StateStore;
use crate::theme;

/// Key the previous eframe-storage implementation used, kept only for a
/// one-time migration into the state file.
const LEGACY_STORAGE_KEY: &str = "rustboard.state.v1";

/// Writes are coalesced for this long so a continuous drag or a burst of
/// typing does not write the state file on every frame.
const SAVE_COALESCE: Duration = Duration::from_millis(250);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedState {
    pub(crate) board: Board,
    pub(crate) camera: Camera,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AppAction {
    AddNote,
    DeleteSelected,
    Select(Option<NoteId>),
    MoveNote { id: NoteId, screen_delta: Vec2 },
    FitView,
    PanBy(Vec2),
    ZoomAt { screen_pos: egui::Pos2, factor: f32 },
}

/// What the toolbar says about saving.
struct SaveStatus {
    label: String,
    color: Color32,
    tooltip: String,
}

pub struct RustboardApp {
    pub(crate) board: Board,
    pub(crate) camera: Camera,
    pub(crate) selected: Option<NoteId>,
    pub(crate) pending_focus: Option<NoteId>,
    pub(crate) pan_button: Option<PointerButton>,
    pub(crate) note_drag: Option<NoteId>,
    pub(crate) fit_pending: bool,
    pub(crate) canvas_rect: Rect,
    store: Option<StateStore>,
    dirty: bool,
    last_save_attempt: Instant,
    last_saved: Option<Instant>,
    save_error: Option<String>,
    load_error: Option<String>,
    legacy_key_present: bool,
}

impl RustboardApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        let mut app = Self::from_parts(Board::seeded(), Camera::default(), true);
        app.store = Some(StateStore::open());
        if let Some(error) = app.store.as_ref().and_then(StateStore::last_error) {
            app.save_error = Some(error.to_owned());
        }

        let legacy = cc
            .storage
            .and_then(|storage| eframe::get_value::<PersistedState>(storage, LEGACY_STORAGE_KEY));
        app.restore(legacy);

        app
    }

    fn from_parts(board: Board, camera: Camera, fit_pending: bool) -> Self {
        Self {
            board,
            camera,
            selected: None,
            pending_focus: None,
            pan_button: None,
            note_drag: None,
            fit_pending,
            canvas_rect: Rect::ZERO,
            store: None,
            dirty: false,
            last_save_attempt: Instant::now(),
            last_saved: None,
            save_error: None,
            load_error: None,
            legacy_key_present: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::from_parts(Board::seeded(), Camera::default(), true)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_store(path: impl Into<std::path::PathBuf>) -> Self {
        let mut app = Self::from_parts(Board::seeded(), Camera::default(), true);
        app.store = Some(StateStore::at(path));
        app
    }

    /// Load the state file, falling back to the old eframe payload.
    fn restore(&mut self, legacy: Option<PersistedState>) {
        let legacy_present = legacy.is_some();

        let mut loaded = None;
        if let Some(store) = &self.store {
            match store.load::<PersistedState>() {
                Ok(state) => loaded = state,
                Err(error) => self.load_error = Some(error),
            }
        }

        let migrated = loaded.is_none() && legacy_present;
        if let Some(mut saved) = loaded.or(legacy) {
            saved.board.sanitize();
            saved.camera.sanitize();
            self.board = saved.board;
            self.camera = saved.camera;
            self.fit_pending = false;
        }

        self.legacy_key_present = legacy_present;
        if migrated {
            // Write the old payload into the state file on the next save.
            self.dirty = true;
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Write the board and camera to disk now, if anything changed.
    pub(crate) fn save_now(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(store) = self.store.as_mut() else {
            return;
        };

        self.last_save_attempt = Instant::now();
        let state = PersistedState {
            board: self.board.clone(),
            camera: self.camera,
        };

        match store.save(&state) {
            Ok(()) => {
                self.dirty = false;
                self.last_saved = Some(Instant::now());
                self.save_error = None;
                self.load_error = None;
            }
            Err(error) => self.save_error = Some(error),
        }
    }

    /// Save shortly after every edit, coalescing bursts into one write.
    fn maybe_autosave(&mut self, ctx: &egui::Context) {
        if !self.dirty || self.store.is_none() {
            return;
        }

        let waited = self.last_save_attempt.elapsed();
        if self.last_saved.is_none() || waited >= SAVE_COALESCE {
            self.save_now();
            if self.dirty {
                // The write failed: keep the indicator visible without spinning.
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        } else {
            ctx.request_repaint_after(SAVE_COALESCE - waited);
        }
    }

    fn save_status(&self) -> SaveStatus {
        let location = self
            .store
            .as_ref()
            .map(|store| store.path().display().to_string())
            .unwrap_or_else(|| "no state file".to_owned());

        if let Some(error) = &self.save_error {
            return SaveStatus {
                label: "Save failed".to_owned(),
                color: theme::SAVE_ERROR,
                tooltip: format!("{error}\n\nClick to retry saving.\nTarget: {location}"),
            };
        }
        if let Some(error) = &self.load_error {
            return SaveStatus {
                label: "Save unreadable".to_owned(),
                color: theme::SAVE_WARN,
                tooltip: format!(
                    "{error}\n\nThe sample board was opened instead; new edits are written to {location}."
                ),
            };
        }
        if self.store.is_none() {
            return SaveStatus {
                label: "Saving off".to_owned(),
                color: theme::TEXT_MUTED,
                tooltip: "This session has no state file.".to_owned(),
            };
        }
        if self.dirty {
            return SaveStatus {
                label: "Saving…".to_owned(),
                color: theme::TEXT_MUTED,
                tooltip: format!("Writing {location}"),
            };
        }
        if self.last_saved.is_some() {
            return SaveStatus {
                label: "Saved".to_owned(),
                color: theme::SAVE_OK,
                tooltip: format!(
                    "Every edit is saved automatically.\n{location}\n\nClick to save now (Ctrl+S)."
                ),
            };
        }

        SaveStatus {
            label: "Auto-save on".to_owned(),
            color: theme::TEXT_MUTED,
            tooltip: format!("Edits are saved automatically to {location}."),
        }
    }

    pub(crate) fn update(&mut self, action: AppAction) {
        match action {
            AppAction::AddNote => {
                let cascade_index = (self.board.notes.len() % 6) as f32;
                let zoom = self.camera.zoom.max(0.01);
                let cascade = [cascade_index * 24.0 / zoom, cascade_index * 18.0 / zoom];
                let center = [self.camera.center[0], self.camera.center[1]];
                let id = self.board.add_note(center, cascade);
                self.selected = Some(id);
                self.pending_focus = Some(id);
                self.mark_dirty();
            }
            AppAction::DeleteSelected => {
                if let Some(id) = self.selected.take() {
                    if self.board.remove(id) {
                        self.mark_dirty();
                    }
                    if self.pending_focus == Some(id) {
                        self.pending_focus = None;
                    }
                }
            }
            AppAction::Select(id) => {
                self.selected = id;
                if let Some(id) = id {
                    let before = self.board.notes.iter().position(|note| note.id == id);
                    self.board.bring_to_front(id);
                    let after = self.board.notes.iter().position(|note| note.id == id);
                    if before != after {
                        self.mark_dirty();
                    }
                }
            }
            AppAction::MoveNote { id, screen_delta } => {
                if screen_delta == Vec2::ZERO {
                    return;
                }
                let zoom = self.camera.zoom.max(0.01);
                if let Some(note) = self.board.notes.iter_mut().find(|note| note.id == id) {
                    note.pos[0] += screen_delta.x / zoom;
                    note.pos[1] += screen_delta.y / zoom;
                    self.mark_dirty();
                }
            }
            AppAction::FitView => {
                self.fit_pending = true;
            }
            AppAction::PanBy(delta) => {
                if delta == Vec2::ZERO {
                    return;
                }
                self.camera.pan_screen(delta);
                self.mark_dirty();
            }
            AppAction::ZoomAt { screen_pos, factor } => {
                let before = self.camera;
                self.camera.zoom_at(screen_pos, self.canvas_rect, factor);
                if self.camera != before {
                    self.mark_dirty();
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // Saving must work even while a note editor has keyboard focus.
        let mut save = false;
        ctx.input_mut(|input| {
            save = input.consume_key(Modifiers::CTRL, Key::S);
        });
        if save {
            self.save_now();
        }

        if ctx.egui_wants_keyboard_input() {
            return;
        }

        let mut add_note = false;
        let mut delete_note = false;
        ctx.input_mut(|input| {
            add_note = input.consume_key(Modifiers::CTRL, Key::N);
            delete_note = input.consume_key(Modifiers::NONE, Key::Delete)
                || input.consume_key(Modifiers::NONE, Key::Backspace);
        });

        if add_note {
            self.update(AppAction::AddNote);
        }
        if delete_note {
            self.update(AppAction::DeleteSelected);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let (icon_rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            ui.painter().rect_filled(icon_rect, 7, theme::ACCENT);
            let mark = Rect::from_center_size(icon_rect.center(), vec2(8.0, 11.0));
            ui.painter()
                .rect_filled(mark, 2, Color32::from_rgb(0x10, 0x17, 0x29));

            ui.add_space(8.0);
            ui.label(
                RichText::new("Rustboard")
                    .size(20.0)
                    .strong()
                    .color(theme::TEXT_PRIMARY),
            );
            ui.add_space(14.0);

            let new_note = egui::Button::new(
                RichText::new("+  New note")
                    .strong()
                    .color(Color32::from_rgb(0x0e, 0x14, 0x24)),
            )
            .fill(theme::ACCENT)
            .corner_radius(8)
            .min_size(vec2(118.0, 32.0));

            if ui
                .add(new_note)
                .on_hover_text("Add a note at the center of the view (Ctrl+N)")
                .clicked()
            {
                self.update(AppAction::AddNote);
            }

            let delete = egui::Button::new("Delete")
                .corner_radius(8)
                .min_size(vec2(76.0, 32.0));
            if ui
                .add_enabled(self.selected.is_some(), delete)
                .on_disabled_hover_text("Select a note first")
                .on_hover_text("Delete the selected note")
                .clicked()
            {
                self.update(AppAction::DeleteSelected);
            }

            let fit = egui::Button::new("Fit view")
                .corner_radius(8)
                .min_size(vec2(82.0, 32.0));
            if ui
                .add(fit)
                .on_hover_text("Frame every note on the board")
                .clicked()
            {
                self.update(AppAction::FitView);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{:.0}%", self.camera.zoom * 100.0))
                        .monospace()
                        .color(theme::TEXT_MUTED),
                )
                .on_hover_text("Zoom level — scroll over the canvas to change it");
                ui.add_space(14.0);
                ui.label(
                    RichText::new(format!(
                        "{} note{}",
                        self.board.notes.len(),
                        if self.board.notes.len() == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(14.0);

                let status = self.save_status();
                let response = ui.add(
                    egui::Label::new(RichText::new(&status.label).size(12.0).color(status.color))
                        .sense(Sense::click()),
                );
                if response.on_hover_text(status.tooltip).clicked() {
                    self.save_now();
                }
            });
        });
    }

    fn hint_bar(&mut self, ui: &mut Ui) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            for (index, hint) in [
                "Scroll to zoom",
                "Right / middle drag to pan",
                "Drag a note header to move",
                "Click note text to edit",
                "Del removes selected",
                "Ctrl+S saves now",
            ]
            .iter()
            .enumerate()
            {
                if index > 0 {
                    ui.label(RichText::new("·").color(theme::TEXT_MUTED).size(12.0));
                }
                ui.label(RichText::new(*hint).size(12.0).color(theme::TEXT_MUTED));
            }
        });
    }
}

impl eframe::App for RustboardApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show_ui(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.legacy_key_present {
            storage.remove_string(LEGACY_STORAGE_KEY);
            self.legacy_key_present = false;
        }
        self.save_now();
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn on_exit(&mut self) {
        self.save_now();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::CANVAS_BG.to_normalized_gamma_f32()
    }
}

impl RustboardApp {
    pub(crate) fn show_ui(&mut self, ui: &mut Ui) {
        self.shortcuts(ui.ctx());

        egui::Panel::top("toolbar")
            .exact_size(58.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::TOOLBAR_BG)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| self.toolbar(ui));

        egui::Panel::bottom("hint_bar")
            .exact_size(30.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::HINT_BG)
                    .inner_margin(egui::Margin::symmetric(14, 6)),
            )
            .show(ui, |ui| self.hint_bar(ui));

        egui::CentralPanel::no_frame().show(ui, |ui| canvas::show(self, ui));

        self.maybe_autosave(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    use eframe::Storage as _;

    use super::*;

    fn temp_state_path(name: &str) -> std::path::PathBuf {
        crate::persist::scratch_dir(name).join("board.ron")
    }

    #[test]
    fn board_and_camera_round_trip_through_the_state_file() {
        let path = temp_state_path("round-trip");
        let mut app = RustboardApp::for_test_with_store(&path);
        app.board.add_note([40.0, -20.0], [24.0, 18.0]);
        app.camera.zoom = 1.6;
        app.camera.center = [120.0, -45.0];
        app.mark_dirty();
        app.save_now();
        assert!(!app.dirty, "a successful save clears the pending flag");

        let expected_board = app.board.clone();
        let expected_camera = app.camera;
        let saved: PersistedState = StateStore::at(&path)
            .load()
            .expect("state file loads")
            .expect("state file exists");
        assert_eq!(saved.board, expected_board);
        assert_eq!(saved.camera, expected_camera);
    }

    #[test]
    fn legacy_eframe_payload_is_restored_and_queued_for_saving() {
        let path = temp_state_path("legacy");
        let mut app = RustboardApp::for_test_with_store(&path);

        let mut legacy = PersistedState {
            board: Board::seeded(),
            camera: Camera {
                center: [10.0, 20.0],
                zoom: 1.25,
            },
        };
        legacy.board.add_note([5.0, 5.0], [0.0, 0.0]);

        app.restore(Some(legacy));
        assert_eq!(app.camera.center, [10.0, 20.0]);
        assert!(app.dirty, "migrated state must be written to the new file");
        assert!(
            app.legacy_key_present,
            "the old storage key should be purged"
        );

        app.save_now();
        let saved: PersistedState = StateStore::at(&path)
            .load()
            .expect("state file loads")
            .expect("state file exists");
        assert_eq!(saved.camera.zoom, 1.25);
        assert_eq!(saved.board.notes.len(), Board::seeded().notes.len() + 1);
    }

    #[test]
    fn edits_mark_the_board_dirty() {
        let mut app = RustboardApp::for_test();
        assert!(!app.dirty);

        app.update(AppAction::AddNote);
        assert!(app.dirty);

        app.dirty = false;
        let id = app.board.notes.last().expect("note").id;
        app.update(AppAction::Select(Some(id)));
        assert!(!app.dirty, "selecting the front note changes nothing");

        app.dirty = false;
        app.update(AppAction::MoveNote {
            id,
            screen_delta: Vec2::ZERO,
        });
        assert!(!app.dirty, "a zero-length move changes nothing");

        app.update(AppAction::MoveNote {
            id,
            screen_delta: vec2(4.0, 4.0),
        });
        assert!(app.dirty);
    }

    #[test]
    fn failed_saves_keep_the_board_pending_and_report_the_error() {
        let dir = crate::persist::scratch_dir("blocked");

        // A file where the parent folder should be makes the target unusable.
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, b"not a folder").expect("blocker file");

        let mut app = RustboardApp::for_test_with_store(blocker.join("board.ron"));
        app.update(AppAction::AddNote);
        app.save_now();

        assert!(app.dirty, "a failed save must stay pending");
        assert!(app.save_error.is_some(), "the failure should be reported");
    }

    #[derive(Default)]
    struct MemoryStorage {
        values: std::collections::HashMap<String, String>,
    }

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.values.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.values.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.values.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn saving_writes_the_file_and_purges_the_legacy_key() {
        let path = temp_state_path("legacy-purge");
        let mut app = RustboardApp::for_test_with_store(&path);
        app.restore(Some(PersistedState {
            board: Board::seeded(),
            camera: Camera::default(),
        }));

        let mut storage = MemoryStorage::default();
        storage.set_string(LEGACY_STORAGE_KEY, "old payload".to_owned());
        eframe::App::save(&mut app, &mut storage);

        assert_eq!(storage.get_string(LEGACY_STORAGE_KEY), None);
        assert!(path.is_file(), "the state file should exist");
        assert!(!app.dirty);
    }
}
