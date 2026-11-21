use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Key, Layout, Modifiers, PointerButton, Rect, RichText, Sense, Ui, Vec2,
    vec2,
};
use serde::{Deserialize, Serialize};

use crate::board::{Board, NoteId};
use crate::camera::Camera;
use crate::canvas;
use crate::theme;

const STORAGE_KEY: &str = "rustboard.state.v1";

#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    board: Board,
    camera: Camera,
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

pub struct RustboardApp {
    pub(crate) board: Board,
    pub(crate) camera: Camera,
    pub(crate) selected: Option<NoteId>,
    pub(crate) pending_focus: Option<NoteId>,
    pub(crate) pan_button: Option<PointerButton>,
    pub(crate) note_drag: Option<NoteId>,
    pub(crate) fit_pending: bool,
    pub(crate) canvas_rect: Rect,
}

impl RustboardApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        let mut app = Self::from_parts(Board::seeded(), Camera::default(), true);

        if let Some(storage) = cc.storage
            && let Some(mut saved) = eframe::get_value::<PersistedState>(storage, STORAGE_KEY)
        {
            saved.board.sanitize();
            saved.camera.sanitize();
            app.board = saved.board;
            app.camera = saved.camera;
            app.fit_pending = false;
        }

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
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::from_parts(Board::seeded(), Camera::default(), true)
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
            }
            AppAction::DeleteSelected => {
                if let Some(id) = self.selected.take() {
                    self.board.remove(id);
                    if self.pending_focus == Some(id) {
                        self.pending_focus = None;
                    }
                }
            }
            AppAction::Select(id) => {
                self.selected = id;
                if let Some(id) = id {
                    self.board.bring_to_front(id);
                }
            }
            AppAction::MoveNote { id, screen_delta } => {
                let zoom = self.camera.zoom.max(0.01);
                if let Some(note) = self.board.notes.iter_mut().find(|note| note.id == id) {
                    note.pos[0] += screen_delta.x / zoom;
                    note.pos[1] += screen_delta.y / zoom;
                }
            }
            AppAction::FitView => {
                self.fit_pending = true;
            }
            AppAction::PanBy(delta) => self.camera.pan_screen(delta),
            AppAction::ZoomAt { screen_pos, factor } => {
                self.camera.zoom_at(screen_pos, self.canvas_rect, factor);
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
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
        let state = PersistedState {
            board: self.board.clone(),
            camera: self.camera,
        };
        eframe::set_value(storage, STORAGE_KEY, &state);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(2)
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct MemoryStorage {
        values: HashMap<String, String>,
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
    fn board_and_camera_round_trip_through_eframe_storage() {
        let mut app = RustboardApp::for_test();
        app.board.add_note([40.0, -20.0], [24.0, 18.0]);
        app.camera.zoom = 1.6;
        app.camera.center = [120.0, -45.0];

        let expected_board = app.board.clone();
        let expected_camera = app.camera;
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);

        let restored: PersistedState =
            eframe::get_value(&storage, STORAGE_KEY).expect("saved app state should deserialize");
        assert_eq!(restored.board, expected_board);
        assert_eq!(restored.camera, expected_camera);
    }
}
