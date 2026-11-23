//! Board canvas for the SQLite model (task 100).
//!
//! The view turns [model::Board] into layout geometry, paints nested
//! containers, notes and arrows, and turns pointer input into [Command] values.
//! It owns no database handle: the app applies the returned commands through
//! [crate::command::Store::apply] and reloads.
//!
//! Interaction rules:
//! * a click selects a note; a click on an already selected note, a
//!   double-click or Enter opens the inline editor;
//! * Shift+click adds or removes a note or arrow; a left-drag on empty canvas
//!   draws a selection box; Ctrl+A selects every visible note;
//! * Delete or Backspace soft-deletes the selection (subtrees included);
//! * Escape saves and closes the editor, then clears the selection; the app
//!   hides the window only when the board did not use the key;
//! * right-drag, middle-drag or Space+left-drag pans;
//! * root notes pin where they are dropped; child notes reparent when dropped
//!   over a container, otherwise they move to the root; a drop on the note
//!   itself or one of its descendants is rejected;
//! * a hovered or single selected note shows a link handle on its right
//!   border; drag it onto another note to link (a live arrow follows);
//! * right-click an arrow to delete it;
//! * each note has a checkbox; Ctrl+Enter toggles done on the selection; done
//!   notes stay in place with struck-through, muted text and sort after their
//!   open siblings; "Hide done" omits done notes that have no open descendants;
//! * containers show "done/total" for their direct children;
//! * snoozed notes are painted at 35% opacity.

use std::collections::HashMap;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Key, Modifiers, PointerButton, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::camera::Camera;
use crate::command::Command;
use crate::layout::{self, EdgeGeometry, Layout, LayoutNote};
use crate::model::{Board, EdgeId, Note, NoteId, Status};
use crate::theme;
use crate::time::TimeSpec;

/// Opacity used for a snoozed note, per the design steer.
pub const SNOOZE_OPACITY: f32 = 0.35;
/// A drop must move at least this many screen pixels to count as a drag.
const DRAG_THRESHOLD: f32 = 3.0;
/// Hit radius around an arrow, in screen pixels.
const EDGE_HIT_RADIUS: f32 = 10.0;
/// Radius of the link handle drawn on the selected note.
const LINK_HANDLE_RADIUS: f32 = 7.0;
/// Height of a container header in world units (matches the layout engine).
const CONTAINER_HEADER: f32 = 36.0;
/// Side of the done checkbox in world units.
const CHECKBOX_SIZE: f32 = 16.0;

/// Everything the view needs from the app for one frame.
pub struct BoardContext<'a> {
    pub board: &'a Board,
    pub now_utc_ms: i64,
    /// IANA zone used to format deadlines and to build snooze values.
    pub tz: &'a str,
    /// Omit done notes that have no open descendants.
    pub hide_done: bool,
}

/// What the frame produced. The app applies the commands in one batch.
#[derive(Clone, Debug)]
pub struct BoardOutput {
    pub commands: Vec<Command>,
    pub canvas_rect: Rect,
    /// True while a note is being edited inline (the app defers reloads).
    pub editing: bool,
    /// Notes the frame asked to delete (roots of the deleted subtrees). The app
    /// uses it to offer Undo.
    pub deleted_notes: usize,
    /// Open the editor on the batch's first new note (a Ctrl+G container).
    pub edit_new_note: bool,
}

impl Default for BoardOutput {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            canvas_rect: Rect::ZERO,
            editing: false,
            deleted_notes: 0,
            edit_new_note: false,
        }
    }
}

/// The camera and the interaction state that survives between frames.
pub struct BoardView {
    pub camera: Camera,
    /// Selected notes, in selection order.
    pub selected: Vec<NoteId>,
    /// Selected arrows, in selection order.
    pub selected_edges: Vec<EdgeId>,
    marquee: Option<Marquee>,
    /// The arrow whose right-click menu is open.
    edge_menu: Option<EdgeId>,
    editing: Option<NoteId>,
    edit_buffer: String,
    focus_requested: Option<NoteId>,
    fit_requested: bool,
    drag: Option<NoteDrag>,
    link: Option<LinkDrag>,
    drop_plan: Option<DropPlan>,
    /// Pan the camera to this note once it has a rect.
    reveal: Option<NoteId>,
    pan_button: Option<PointerButton>,
}

#[derive(Clone, Debug)]
struct NoteDrag {
    id: NoteId,
    /// Pointer offset from the note's top-left, in world units.
    grab: Vec2,
    moved: bool,
    /// The note was already the only selection when the press started, so a
    /// click without movement opens the editor.
    reselect: bool,
}

/// Where a drag will land, recomputed every frame while dragging.
#[derive(Clone, Debug, PartialEq)]
enum DropPlan {
    /// Append inside this note (it becomes or stays a container).
    Nest { target: NoteId },
    /// Insert among `parent`'s children, before `before` (None = last).
    /// `marker` is the note whose edge shows the insertion line.
    Place {
        parent: Option<NoteId>,
        before: Option<NoteId>,
        marker: NoteId,
        after: bool,
    },
    /// Pin at the root with the dragged note's top-left here (world units).
    Pin { x: f32, y: f32 },
}

/// A selection box drawn by a left-drag on empty canvas.
#[derive(Clone, Debug)]
struct Marquee {
    /// Press position in screen coordinates.
    origin: Pos2,
    /// Shift was held: the box adds to the existing selection.
    additive: bool,
    /// The selection when the drag started (kept for additive boxes).
    base: Vec<NoteId>,
}

#[derive(Clone, Debug)]
struct LinkDrag {
    from: NoteId,
}

impl Default for BoardView {
    fn default() -> Self {
        Self::new()
    }
}

impl BoardView {
    pub fn new() -> Self {
        Self {
            camera: Camera::default(),
            selected: Vec::new(),
            selected_edges: Vec::new(),
            marquee: None,
            edge_menu: None,
            editing: None,
            edit_buffer: String::new(),
            focus_requested: None,
            fit_requested: false,
            drag: None,
            link: None,
            drop_plan: None,
            reveal: None,
            pan_button: None,
        }
    }

    /// Frame every note on the next frame.
    pub fn fit_next_frame(&mut self) {
        self.fit_requested = true;
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Stop the inline editor without emitting a command (used by the app when
    /// an external change forces a reload, and by tests).
    pub fn stop_editing(&mut self) {
        self.editing = None;
        self.edit_buffer.clear();
        self.focus_requested = None;
    }

    pub fn editing_note(&self) -> Option<&NoteId> {
        self.editing.as_ref()
    }

    pub fn camera(&self) -> Camera {
        self.camera
    }

    pub fn set_camera(&mut self, camera: Camera) {
        self.camera = camera;
    }

    /// The single selected note, if exactly one note and no arrow is selected.
    pub fn single_selected(&self) -> Option<&NoteId> {
        match (self.selected.as_slice(), self.selected_edges.is_empty()) {
            ([only], true) => Some(only),
            _ => None,
        }
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selected.iter().any(|selected| selected == id)
    }

    /// Replace the selection with one note.
    pub fn select_only(&mut self, id: NoteId) {
        self.selected = vec![id];
        self.selected_edges.clear();
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.selected_edges.clear();
    }

    fn toggle_note(&mut self, id: NoteId) {
        if let Some(index) = self.selected.iter().position(|selected| *selected == id) {
            self.selected.remove(index);
        } else {
            self.selected.push(id);
        }
    }

    fn toggle_edge(&mut self, id: EdgeId) {
        if let Some(index) = self.selected_edges.iter().position(|edge| *edge == id) {
            self.selected_edges.remove(index);
        } else {
            self.selected_edges.push(id);
        }
    }

    /// Pan to a note on the next frame where it has a rect (new notes).
    pub fn reveal(&mut self, id: NoteId) {
        self.reveal = Some(id);
    }

    /// Clear transient state after a board reload.
    pub fn after_reload(&mut self, board: &Board) {
        self.selected.retain(|id| board.note(id).is_some());
        self.selected_edges.retain(|id| board.edge(id).is_some());
        if let Some(id) = &self.editing
            && board.note(id).is_none()
        {
            self.editing = None;
        }
    }

    /// Start editing a note's text.
    pub fn edit_note(&mut self, board: &Board, id: NoteId) {
        self.select_only(id.clone());
        self.editing = Some(id.clone());
        self.edit_buffer = board
            .note(&id)
            .map(|note| note.text.clone())
            .unwrap_or_default();
        self.focus_requested = Some(id);
    }

    /// Screen rect of a note for the given canvas (test seam for the app tests).
    #[cfg(test)]
    pub(crate) fn screen_rect_for(&self, board: &Board, id: &str, canvas: Rect) -> Option<Rect> {
        Geometry::build(board, 0, false).screen_rect(id, &self.camera, canvas)
    }

    /// Centre of a note's done checkbox (test seam for the app tests).
    #[cfg(test)]
    pub(crate) fn checkbox_center_for(
        &self,
        board: &Board,
        id: &str,
        canvas: Rect,
    ) -> Option<Pos2> {
        let screen = self.screen_rect_for(board, id, canvas)?;
        Some(checkbox_rect(header_rect(screen, self.camera.zoom), self.camera.zoom).center())
    }

    /// Draw the board and return the commands the input produced.
    pub fn show(&mut self, ui: &mut Ui, context: &BoardContext<'_>) -> BoardOutput {
        let available = ui.available_size();
        let mut output = BoardOutput::default();
        if !available.x.is_finite()
            || !available.y.is_finite()
            || available.x < 1.0
            || available.y < 1.0
        {
            return output;
        }

        let (canvas_rect, background) = ui.allocate_exact_size(available, Sense::click());
        if canvas_rect.width() < 1.0 || canvas_rect.height() < 1.0 {
            return output;
        }
        output.canvas_rect = canvas_rect;

        let geometry = Geometry::build(context.board, context.now_utc_ms, context.hide_done);

        if self.fit_requested {
            self.fit_requested = false;
            if let Some(bounds) = geometry.layout.bounds() {
                self.camera.fit_to_bounds(world_rect(bounds), canvas_rect);
            } else {
                self.camera.reset_to_origin();
            }
        }

        if let Some(id) = self.reveal.take()
            && let Some(rect) = geometry.screen_rect(&id, &self.camera, canvas_rect)
            && !canvas_rect.shrink(24.0).contains_rect(rect)
        {
            self.camera.pan_screen(canvas_rect.center() - rect.center());
        }

        self.handle_wheel(ui, canvas_rect);
        self.handle_pan(ui, canvas_rect);
        self.handle_keys(ui, context, &geometry, &mut output);
        self.handle_note_input(ui, context, &geometry, canvas_rect, &mut output);
        self.handle_link_input(ui, context, &geometry, canvas_rect, &mut output);

        let painter = ui.painter_at(canvas_rect);
        painter.rect_filled(canvas_rect, 0, theme::CANVAS_BG);
        paint_grid(&painter, &self.camera, canvas_rect);
        let hovered_edge = self.hovered_edge(ui, context, &geometry, canvas_rect);
        if hovered_edge.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        paint_edges(
            &painter,
            context.board,
            &geometry,
            &self.camera,
            canvas_rect,
            &self.selected_edges,
            hovered_edge.as_deref(),
        );
        self.paint_notes(ui, &painter, context, &geometry, canvas_rect, &mut output);
        self.paint_link_hint(ui, context, &geometry, canvas_rect);
        self.paint_drop_hint(ui, &geometry, canvas_rect);
        self.paint_marquee(ui, canvas_rect);

        if context.board.notes.is_empty() {
            painter.text(
                canvas_rect.center() + vec2(0.0, -10.0),
                Align2::CENTER_CENTER,
                "Your board is empty",
                FontId::proportional(22.0),
                theme::TEXT_PRIMARY,
            );
            painter.text(
                canvas_rect.center() + vec2(0.0, 20.0),
                Align2::CENTER_CENTER,
                "Type a line in the composer below and press Enter",
                FontId::proportional(13.0),
                theme::TEXT_MUTED,
            );
        }

        self.edge_context_menu(
            ui,
            context,
            &geometry,
            canvas_rect,
            &background,
            &mut output,
        );

        if self.drag.is_some() || self.pan_button.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }

        output.editing = self.editing.is_some();
        output
    }

    fn handle_wheel(&mut self, ui: &mut Ui, canvas_rect: Rect) {
        let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) else {
            return;
        };
        if !canvas_rect.contains(pointer) || !on_board_layer(ui, pointer) {
            return;
        }
        let scroll = ui.ctx().input_mut(|input| {
            let delta = input.smooth_scroll_delta;
            input.smooth_scroll_delta = Vec2::ZERO;
            delta
        });
        if scroll.y.abs() < 0.01 {
            return;
        }
        let factor = (scroll.y * 0.0025).exp().clamp(0.5, 2.0);
        self.camera.zoom_at(pointer, canvas_rect, factor);
        ui.ctx().request_repaint();
    }

    fn handle_pan(&mut self, ui: &mut Ui, canvas_rect: Rect) {
        let typing = ui.ctx().memory(|memory| memory.focused().is_some());
        let (
            primary_pressed,
            secondary_pressed,
            middle_pressed,
            primary_down,
            secondary_down,
            middle_down,
            space_down,
            pointer,
            delta,
        ) = ui.input(|input| {
            (
                input.pointer.button_pressed(PointerButton::Primary),
                input.pointer.button_pressed(PointerButton::Secondary),
                input.pointer.button_pressed(PointerButton::Middle),
                input.pointer.primary_down(),
                input.pointer.secondary_down(),
                input.pointer.middle_down(),
                input.key_down(egui::Key::Space),
                input.pointer.latest_pos(),
                input.pointer.delta(),
            )
        });
        let space_pan = space_down && !typing;
        if space_pan && self.pan_button.is_none() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }

        if self.pan_button.is_none()
            && let Some(pointer) = pointer
            && canvas_rect.contains(pointer)
            && on_board_layer(ui, pointer)
        {
            if primary_pressed && space_pan {
                self.pan_button = Some(PointerButton::Primary);
            } else if secondary_pressed {
                self.pan_button = Some(PointerButton::Secondary);
            } else if middle_pressed {
                self.pan_button = Some(PointerButton::Middle);
            }
        }

        let Some(button) = self.pan_button else {
            return;
        };
        let still_down = match button {
            PointerButton::Primary => primary_down,
            PointerButton::Secondary => secondary_down,
            PointerButton::Middle => middle_down,
            _ => false,
        };
        if !still_down {
            self.pan_button = None;
            return;
        }
        if delta != Vec2::ZERO {
            self.camera.pan_screen(delta);
            ui.ctx().request_repaint();
        }
    }

    fn handle_note_input(
        &mut self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
        output: &mut BoardOutput,
    ) {
        if self.pan_button.is_some() {
            return;
        }
        let pointer = ui.input(|input| input.pointer.interact_pos());
        let primary_down = ui.input(|input| input.pointer.primary_down());
        let primary_released =
            ui.input(|input| input.pointer.button_released(PointerButton::Primary));

        if let Some(drag) = &mut self.drag {
            if primary_down {
                if let Some(pointer) = pointer {
                    let world = self.camera.screen_to_world(pointer, canvas_rect);
                    let world_rect = geometry
                        .world_rect(&drag.id)
                        .unwrap_or_else(|| Rect::from_min_size(world, Vec2::ZERO));
                    if (world - world_rect.min - drag.grab).length() * self.camera.zoom
                        > DRAG_THRESHOLD
                    {
                        drag.moved = true;
                    }
                    if drag.moved {
                        let grab = drag.grab;
                        let dragged = self.dragged_notes(context.board);
                        self.drop_plan =
                            Some(plan_drop(context.board, geometry, &dragged, world, grab));
                    }
                }
            } else {
                let drag = drag.clone();
                if drag.moved {
                    if let Some(pointer) = pointer {
                        let world = self.camera.screen_to_world(pointer, canvas_rect);
                        output.commands.extend(self.commit_note_drop(
                            context.board,
                            geometry,
                            world,
                        ));
                    }
                } else if drag.reselect && primary_released {
                    self.edit_note(context.board, drag.id.clone());
                }
                self.drag = None;
                self.drop_plan = None;
            }
            ui.ctx().request_repaint();
            return;
        }

        if let Some(marquee) = self.marquee.clone() {
            if primary_down {
                if let Some(pointer) = pointer {
                    let rect = Rect::from_two_pos(marquee.origin, pointer);
                    let mut selection = if marquee.additive {
                        marquee.base.clone()
                    } else {
                        Vec::new()
                    };
                    for id in self.notes_in_rect(geometry, context, rect, canvas_rect) {
                        if !selection.contains(&id) {
                            selection.push(id);
                        }
                    }
                    self.selected = selection;
                }
                ui.ctx().request_repaint();
            } else {
                self.marquee = None;
            }
            return;
        }

        if self.editing.is_some() && self.focus_requested.is_none() && !self.has_editor_focus(ui) {
            self.finish_edit(context.board, true, output);
        }

        let Some(pointer) = pointer else {
            return;
        };
        if !canvas_rect.contains(pointer) || !on_board_layer(ui, pointer) {
            return;
        }
        let (pressed, double, shift) = ui.input(|input| {
            (
                input.pointer.button_pressed(PointerButton::Primary),
                input.pointer.button_double_clicked(PointerButton::Primary),
                input.modifiers.shift,
            )
        });
        if !pressed && !double {
            return;
        }
        // A press inside the open editor belongs to the text field.
        if let Some(editing) = &self.editing
            && self
                .hit_note(geometry, context, pointer, canvas_rect)
                .as_ref()
                == Some(editing)
        {
            return;
        }
        if double {
            if let Some(id) = self.hit_note(geometry, context, pointer, canvas_rect) {
                self.drag = None;
                self.edit_note(context.board, id);
            }
            return;
        }

        self.finish_edit(context.board, true, output);
        surrender_focus(ui.ctx());

        if let Some(id) = self.checkbox_at(geometry, context, pointer, canvas_rect) {
            output.commands.extend(toggle_done_commands(
                context.board,
                std::slice::from_ref(&id),
            ));
            return;
        }

        if let Some(id) = self.important_slot_at(geometry, context, pointer, canvas_rect) {
            output.commands.extend(toggle_important_commands(
                context.board,
                std::slice::from_ref(&id),
            ));
            return;
        }

        if let Some(id) = self.link_handle_at(geometry, context, pointer, canvas_rect) {
            self.link = Some(LinkDrag { from: id });
            return;
        }

        if let Some(id) = self.hit_note(geometry, context, pointer, canvas_rect) {
            if shift {
                self.toggle_note(id);
                return;
            }
            let reselect = self.single_selected() == Some(&id);
            if !self.is_selected(&id) {
                self.select_only(id.clone());
            }
            let world = self.camera.screen_to_world(pointer, canvas_rect);
            let note_rect = geometry
                .world_rect(&id)
                .unwrap_or_else(|| Rect::from_min_size(world, Vec2::ZERO));
            self.drag = Some(NoteDrag {
                id,
                grab: world - note_rect.min,
                moved: false,
                reselect,
            });
            return;
        }

        if let Some(edge) = hit_edge(context.board, geometry, &self.camera, canvas_rect, pointer) {
            if shift {
                self.toggle_edge(edge);
            } else {
                self.selected.clear();
                self.selected_edges = vec![edge];
            }
            return;
        }

        if !shift {
            self.clear_selection();
        }
        self.marquee = Some(Marquee {
            origin: pointer,
            additive: shift,
            base: self.selected.clone(),
        });
    }

    /// Visible notes whose screen rect lies fully inside the box.
    fn notes_in_rect(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        rect: Rect,
        canvas_rect: Rect,
    ) -> Vec<NoteId> {
        if rect.width() < DRAG_THRESHOLD && rect.height() < DRAG_THRESHOLD {
            return Vec::new();
        }
        context
            .board
            .notes
            .iter()
            .filter(|note| {
                geometry
                    .screen_rect(&note.id, &self.camera, canvas_rect)
                    .is_some_and(|screen| rect.contains_rect(screen))
            })
            .map(|note| note.id.clone())
            .collect()
    }

    /// The note whose done checkbox is under the pointer.
    fn checkbox_at(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        let id = self.hit_note(geometry, context, pointer, canvas_rect)?;
        if context.board.note(&id).is_some_and(|note| note.inbox) {
            return None;
        }
        let screen = geometry.screen_rect(&id, &self.camera, canvas_rect)?;
        let header = header_rect(screen, self.camera.zoom);
        checkbox_rect(header, self.camera.zoom)
            .expand(2.0)
            .contains(pointer)
            .then_some(id)
    }

    /// The note whose "!" slot is under the pointer.
    fn important_slot_at(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        let id = self.hit_note(geometry, context, pointer, canvas_rect)?;
        if context.board.note(&id).is_some_and(|note| note.inbox) {
            return None;
        }
        let screen = geometry.screen_rect(&id, &self.camera, canvas_rect)?;
        important_rect(header_rect(screen, self.camera.zoom), self.camera.zoom)
            .expand(2.0)
            .contains(pointer)
            .then_some(id)
    }

    /// The note whose link handle is under the pointer. Handles show on the
    /// hovered note and on a single selected note; the smallest note wins.
    fn link_handle_at(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        let mut best: Option<(f32, NoteId)> = None;
        for note in &context.board.notes {
            if self.editing.as_ref() == Some(&note.id) {
                continue;
            }
            let Some(screen) = geometry.screen_rect(&note.id, &self.camera, canvas_rect) else {
                continue;
            };
            if (pointer - link_handle_center(screen)).length() > LINK_HANDLE_RADIUS * 1.5 {
                continue;
            }
            let area = screen.width() * screen.height();
            if best.as_ref().is_none_or(|(best, _)| area < *best) {
                best = Some((area, note.id.clone()));
            }
        }
        best.map(|(_, id)| id)
    }

    /// The note whose link handle should be drawn: the one under the handle,
    /// else the innermost note under the pointer.
    fn handle_owner(
        &self,
        ui: &Ui,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        if self.drag.is_some() || self.link.is_some() || self.marquee.is_some() {
            return None;
        }
        let pointer = ui.input(|input| input.pointer.hover_pos())?;
        if !on_board_layer(ui, pointer) {
            return None;
        }
        self.link_handle_at(geometry, context, pointer, canvas_rect)
            .or_else(|| self.hit_note(geometry, context, pointer, canvas_rect))
    }

    /// The arrow under the pointer, when no note is.
    fn hovered_edge(
        &self,
        ui: &Ui,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
    ) -> Option<EdgeId> {
        if self.drag.is_some() || self.link.is_some() || self.marquee.is_some() {
            return None;
        }
        let pointer = ui.input(|input| input.pointer.hover_pos())?;
        if !canvas_rect.contains(pointer)
            || !on_board_layer(ui, pointer)
            || self
                .hit_note(geometry, context, pointer, canvas_rect)
                .is_some()
        {
            return None;
        }
        hit_edge(context.board, geometry, &self.camera, canvas_rect, pointer)
    }

    /// Right-click on an arrow opens a small menu for it.
    fn edge_context_menu(
        &mut self,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
        background: &egui::Response,
        output: &mut BoardOutput,
    ) {
        if background.secondary_clicked()
            && let Some(pointer) = background.interact_pointer_pos()
            && self
                .hit_note(geometry, context, pointer, canvas_rect)
                .is_none()
        {
            self.edge_menu = hit_edge(context.board, geometry, &self.camera, canvas_rect, pointer);
            if let Some(edge) = &self.edge_menu {
                self.selected.clear();
                self.selected_edges = vec![edge.clone()];
            }
        }
        let Some(edge) = self.edge_menu.clone() else {
            return;
        };
        let mut delete = false;
        let shown = background.context_menu(|ui| {
            if ui
                .button(RichText::new("Delete link").color(theme::SAVE_ERROR))
                .on_hover_text("Delete or Backspace")
                .clicked()
            {
                delete = true;
                ui.close();
            }
        });
        if delete && context.board.edge(&edge).is_some() {
            output.commands.push(Command::Unlink { edge_id: edge });
            self.selected_edges.clear();
            self.edge_menu = None;
        } else if shown.is_none() {
            self.edge_menu = None;
        }
    }

    fn has_editor_focus(&self, ui: &Ui) -> bool {
        ui.memory(|memory| memory.focused().is_some())
    }

    fn finish_edit(&mut self, board: &Board, keep: bool, output: &mut BoardOutput) {
        let Some(id) = self.editing.take() else {
            return;
        };
        let buffer = std::mem::take(&mut self.edit_buffer);
        if keep
            && let Some(note) = board.note(&id)
            && note.text != buffer
        {
            output.commands.push(Command::SetText { id, text: buffer });
        }
    }

    fn handle_link_input(
        &mut self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
        output: &mut BoardOutput,
    ) {
        let Some(from) = self.link.clone() else {
            return;
        };
        let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        if ui.input(|input| input.pointer.primary_down()) {
            ui.ctx().request_repaint();
            return;
        }
        if let Some(target) = self.hit_note(geometry, context, pointer, canvas_rect)
            && target != from.from
        {
            output.commands.push(Command::Link {
                from: from.from,
                to: target,
                kind: None,
            });
        }
        self.link = None;
    }

    /// Board keys. They act only when no text field has keyboard focus, except
    /// Escape, which also closes the inline editor.
    fn handle_keys(
        &mut self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        output: &mut BoardOutput,
    ) {
        let ctx = ui.ctx().clone();
        let focused = ctx.memory(|memory| memory.focused());
        let editor_focused = self.editing.is_some()
            && focused.is_some_and(|focused| Some(focused) == self.editor_id(ui));

        if self.editing.is_some()
            && (editor_focused || focused.is_none())
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.finish_edit(context.board, true, output);
            surrender_focus(&ctx);
            return;
        }
        if focused.is_some() || self.editing.is_some() {
            return;
        }

        let shift_ctrl = Modifiers::COMMAND | Modifiers::SHIFT;
        let (ungroup, group, important) = ctx.input_mut(|input| {
            (
                input.consume_key(shift_ctrl, Key::G),
                input.consume_key(Modifiers::COMMAND, Key::G),
                input.consume_key(Modifiers::COMMAND, Key::I),
            )
        });
        if group {
            self.group_selection(context.board, output);
        }
        if ungroup {
            output
                .commands
                .extend(ungroup_commands(context.board, &self.selected));
        }
        if important {
            output
                .commands
                .extend(toggle_important_commands(context.board, &self.selected));
        }
        let (escape, delete, enter, select_all, toggle_done) = ctx.input_mut(|input| {
            let escape = !self.selected.is_empty() || !self.selected_edges.is_empty();
            let escape = escape && input.consume_key(Modifiers::NONE, Key::Escape);
            let delete = input.consume_key(Modifiers::NONE, Key::Delete)
                | input.consume_key(Modifiers::NONE, Key::Backspace);
            let toggle_done = input.consume_key(Modifiers::COMMAND, Key::Enter);
            let enter = input.consume_key(Modifiers::NONE, Key::Enter);
            let select_all = input.consume_key(Modifiers::COMMAND, Key::A);
            (escape, delete, enter, select_all, toggle_done)
        });

        if escape {
            self.clear_selection();
        }
        if delete {
            let (commands, notes) = self.delete_selection(context.board);
            output.deleted_notes += notes;
            output.commands.extend(commands);
            ctx.request_repaint();
        }
        if enter && let Some(id) = self.single_selected().cloned() {
            self.edit_note(context.board, id);
        }
        if toggle_done {
            output
                .commands
                .extend(toggle_done_commands(context.board, &self.selected));
        }
        if select_all {
            self.selected = context
                .board
                .notes
                .iter()
                .filter(|note| geometry.world_rect(&note.id).is_some())
                .map(|note| note.id.clone())
                .collect();
            self.selected_edges.clear();
        }
    }

    /// Ctrl+G: wrap the selection (minus the Inbox and descendants of other
    /// selected notes) in a new container and open its title for editing.
    fn group_selection(&mut self, board: &Board, output: &mut BoardOutput) {
        let ids: Vec<NoteId> = self
            .selected
            .iter()
            .filter(|id| board.note(id).is_some_and(|note| !note.inbox))
            .filter(|id| {
                !self
                    .selected
                    .iter()
                    .any(|other| other != *id && is_ancestor_or_self(board, other, id))
            })
            .cloned()
            .collect();
        if ids.is_empty() {
            return;
        }
        output.commands.push(Command::Wrap {
            ids,
            text: String::new(),
        });
        output.edit_new_note = true;
    }

    fn editor_id(&self, ui: &Ui) -> Option<egui::Id> {
        self.editing
            .as_ref()
            .map(|id| ui.scope_id().with(("board_editor", id.clone())))
    }

    /// Commands that delete the selection: arrows first, then each selected
    /// note whose ancestors are not also selected (the subtree goes with it).
    /// Returns the commands and the number of notes asked for.
    fn delete_selection(&mut self, board: &Board) -> (Vec<Command>, usize) {
        let mut commands: Vec<Command> = self
            .selected_edges
            .iter()
            .filter(|id| board.edge(id).is_some())
            .map(|id| Command::Unlink {
                edge_id: id.clone(),
            })
            .collect();
        let roots: Vec<NoteId> = self
            .selected
            .iter()
            .filter(|id| board.note(id).is_some_and(|note| !note.inbox))
            .filter(|id| {
                !self
                    .selected
                    .iter()
                    .any(|other| other != *id && is_ancestor_or_self(board, other, id))
            })
            .cloned()
            .collect();
        let notes = roots.len();
        commands.extend(roots.into_iter().map(|id| Command::Delete { id }));
        self.clear_selection();
        (commands, notes)
    }

    /// The innermost visible note under the pointer.
    fn hit_note(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        let mut best: Option<(f32, NoteId)> = None;
        for note in &context.board.notes {
            if let Some(rect) = geometry.screen_rect(&note.id, &self.camera, canvas_rect)
                && rect.contains(pointer)
            {
                let area = rect.width() * rect.height();
                if best.as_ref().is_none_or(|(best, _)| area < *best) {
                    best = Some((area, note.id.clone()));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    /// The notes a drag moves: the whole selection when the pressed note is
    /// part of it, otherwise just that note. Descendants of other moved notes
    /// are dropped because they travel with their ancestor.
    fn dragged_notes(&self, board: &Board) -> Vec<NoteId> {
        let Some(drag) = &self.drag else {
            return Vec::new();
        };
        let mut ids = if self.is_selected(&drag.id) {
            let mut ids = vec![drag.id.clone()];
            ids.extend(self.selected.iter().filter(|id| **id != drag.id).cloned());
            ids
        } else {
            vec![drag.id.clone()]
        };
        ids.retain(|id| board.note(id).is_some());
        let all = ids.clone();
        ids.retain(|id| {
            !all.iter()
                .any(|other| other != id && is_ancestor_or_self(board, other, id))
        });
        ids
    }

    /// Commit a note drag at the given world position. Used by the release
    /// handler and by the interaction tests.
    fn commit_note_drop(
        &mut self,
        board: &Board,
        geometry: &Geometry,
        drop_world: Pos2,
    ) -> Vec<Command> {
        let Some(drag) = self.drag.clone() else {
            return Vec::new();
        };
        let dragged = self.dragged_notes(board);
        let plan = plan_drop(board, geometry, &dragged, drop_world, drag.grab);
        drop_commands(board, geometry, &dragged, &plan)
    }

    fn paint_notes(
        &mut self,
        ui: &mut Ui,
        painter: &egui::Painter,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
        output: &mut BoardOutput,
    ) {
        let handle_owner = self.handle_owner(ui, geometry, context, canvas_rect);
        // Containers paint first so their children are drawn on top.
        let mut order: Vec<&Note> = context.board.notes.iter().collect();
        order.sort_by_key(|note| !geometry.is_container(&note.id));

        for note in order {
            let Some(screen) = geometry.screen_rect(&note.id, &self.camera, canvas_rect) else {
                continue;
            };
            if !canvas_rect.intersects(screen) {
                continue;
            }
            let opacity = opacity_for(note, context.now_utc_ms);
            let palette = theme::note_palette(note.color.clamp(0, 255) as u8);
            let selected = self.is_selected(&note.id);
            let container = geometry.is_container(&note.id);
            let header = header_rect(screen, self.camera.zoom);
            let done = note.status == Status::Done;

            paint_card(
                painter,
                screen,
                header,
                theme::CARD_CORNER,
                palette,
                selected,
                opacity,
            );

            let title = first_line(&note.text);
            let title_font = (theme::NOTE_TEXT_SIZE * self.camera.zoom * 0.9).max(4.0);
            let text_color = if done {
                dim(palette.ink, opacity * 0.45)
            } else {
                dim(palette.ink, opacity)
            };
            let padding = (theme::NOTE_PADDING * self.camera.zoom).max(2.0);
            let checkbox = checkbox_rect(header, self.camera.zoom);
            let title_left = if note.inbox {
                screen.left() + padding
            } else {
                paint_checkbox(painter, checkbox, done, palette.ink, opacity);
                checkbox.right() + padding * 0.75
            };
            let bang = important_rect(header, self.camera.zoom);
            let bang_hovered = ui
                .input(|input| input.pointer.hover_pos())
                .is_some_and(|pointer| bang.expand(2.0).contains(pointer));
            if !note.inbox && (note.important || bang_hovered) {
                let color = if note.important {
                    dim(theme::IMPORTANT, opacity)
                } else {
                    dim(theme::IMPORTANT, opacity * 0.35)
                };
                painter.text(
                    bang.center(),
                    Align2::CENTER_CENTER,
                    "!",
                    FontId::proportional((title_font * 1.25).max(4.0)),
                    color,
                );
            }
            if bang_hovered && !note.inbox {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            let progress = container
                .then(|| child_progress(context.board, &note.id))
                .flatten();
            let progress_width = if let Some((closed, total)) = progress {
                let galley = painter.layout_no_wrap(
                    format!("{closed}/{total}"),
                    FontId::proportional(title_font * 0.85),
                    dim(palette.ink, opacity * 0.6),
                );
                let width = galley.size().x;
                painter.galley(
                    pos2(
                        bang.left() - padding * 0.5 - width,
                        header.center().y - galley.size().y * 0.5,
                    ),
                    galley,
                    Color32::TRANSPARENT,
                );
                width + padding
            } else {
                0.0
            };
            let label = if title.is_empty() { "Untitled" } else { title };
            let max_width = (bang.left() - padding * 0.5 - progress_width - title_left).max(0.0);
            let galley = painter.layout(
                label.to_owned(),
                FontId::proportional(title_font),
                text_color,
                f32::INFINITY,
            );
            let title_rect = Rect::from_min_size(
                pos2(title_left, header.center().y - galley.size().y * 0.5),
                vec2(galley.size().x.min(max_width), galley.size().y),
            );
            painter
                .with_clip_rect(title_rect.intersect(canvas_rect))
                .galley(title_rect.min, galley, text_color);
            if done && title_rect.width() > 0.0 {
                painter.hline(
                    title_rect.x_range(),
                    title_rect.center().y,
                    Stroke::new((1.2 * self.camera.zoom).clamp(0.6, 2.0), text_color),
                );
            }

            if !container {
                paint_due_chip(painter, screen, note, self.camera.zoom, opacity);
                if is_snoozed(note, context.now_utc_ms) {
                    paint_snooze_mark(painter, screen, self.camera.zoom, opacity);
                }
            }

            // The card's context menu registers before the editor so the text
            // field stays on top for clicks and selection.
            let card_id = ui.scope_id().with(("note_card", note.id.clone()));
            let response = ui.interact(screen, card_id, Sense::click());
            if response.secondary_clicked() && !self.is_selected(&note.id) {
                self.select_only(note.id.clone());
            }
            let targets: Vec<NoteId> = if self.is_selected(&note.id) {
                self.selected.clone()
            } else {
                vec![note.id.clone()]
            };
            response.context_menu(|ui| {
                self.context_menu(ui, context, note, &targets, output);
            });

            if self.editing.as_deref() == Some(note.id.as_str()) {
                let bottom = if container {
                    header.bottom() - padding * 0.5
                } else {
                    (screen.bottom() - padding).max(header.bottom() + padding + 4.0)
                };
                let top = if container {
                    header.top() + padding * 0.5
                } else {
                    header.bottom() + padding
                };
                let editor_rect = Rect::from_min_max(
                    pos2(screen.left() + padding, top),
                    pos2(screen.right() - padding, bottom),
                );
                if editor_rect.width() > 8.0 && editor_rect.height() > 8.0 {
                    let editor_id = ui.scope_id().with(("board_editor", note.id.clone()));
                    let font_size = (theme::NOTE_TEXT_SIZE * self.camera.zoom).max(4.0);
                    let response = ui.put(
                        editor_rect,
                        egui::TextEdit::multiline(&mut self.edit_buffer)
                            .id(editor_id)
                            .frame(egui::Frame::NONE)
                            .background_color(if container {
                                palette.header
                            } else {
                                Color32::TRANSPARENT
                            })
                            .text_color(palette.ink)
                            .font(FontId::proportional(font_size))
                            .desired_width(editor_rect.width()),
                    );
                    if self.focus_requested.as_deref() == Some(note.id.as_str()) {
                        response.request_focus();
                        self.focus_requested = None;
                    }
                    if response.has_focus() {
                        ui.ctx().request_repaint();
                    }
                }
            } else if self.single_selected().map(String::as_str) == Some(note.id.as_str())
                || handle_owner.as_deref() == Some(note.id.as_str())
            {
                let center = link_handle_center(screen);
                painter.circle_filled(center, LINK_HANDLE_RADIUS, dim(theme::ACCENT, opacity));
                let hovered = ui
                    .input(|input| input.pointer.hover_pos())
                    .is_some_and(|pointer| (pointer - center).length() <= LINK_HANDLE_RADIUS * 1.5);
                if hovered {
                    ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                }
            }
        }
    }

    fn context_menu(
        &mut self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        note: &Note,
        targets: &[NoteId],
        output: &mut BoardOutput,
    ) {
        let many = targets.len() > 1;
        if !many && ui.button("Edit").clicked() {
            self.edit_note(context.board, note.id.clone());
            ui.close();
        }
        let protected = targets
            .iter()
            .all(|id| context.board.note(id).is_some_and(|note| note.inbox));
        let all_done = targets.iter().all(|id| {
            context
                .board
                .note(id)
                .is_some_and(|note| note.status == Status::Done)
        });
        let label = match (all_done, many) {
            (true, _) => "Reopen",
            (false, true) => "Mark all done",
            (false, false) => "Mark done",
        };
        if !protected && ui.button(label).on_hover_text("Ctrl+Enter").clicked() {
            output
                .commands
                .extend(toggle_done_commands(context.board, targets));
            ui.close();
        }
        if !protected {
            let all_important = targets
                .iter()
                .all(|id| context.board.note(id).is_some_and(|note| note.important));
            let label = if all_important {
                "Not important"
            } else {
                "Mark important"
            };
            if ui.button(label).on_hover_text("Ctrl+I").clicked() {
                output
                    .commands
                    .extend(toggle_important_commands(context.board, targets));
                ui.close();
            }
        }
        if ui.button("Snooze 1 hour").clicked() {
            if let Some(until) = snooze_after(context.now_utc_ms, 60, context.tz) {
                output
                    .commands
                    .extend(targets.iter().map(|id| Command::Snooze {
                        id: id.clone(),
                        until: until.clone(),
                    }));
            }
            ui.close();
        }
        if ui.button("Snooze until tomorrow 09:00").clicked() {
            if let Some(until) = snooze_tomorrow(context.now_utc_ms, context.tz) {
                output
                    .commands
                    .extend(targets.iter().map(|id| Command::Snooze {
                        id: id.clone(),
                        until: until.clone(),
                    }));
            }
            ui.close();
        }
        let snoozed: Vec<NoteId> = targets
            .iter()
            .filter(|id| {
                context
                    .board
                    .note(id)
                    .is_some_and(|note| note.snooze.is_some())
            })
            .cloned()
            .collect();
        if !snoozed.is_empty() && ui.button("Unsnooze").clicked() {
            output
                .commands
                .extend(snoozed.into_iter().map(|id| Command::Unsnooze { id }));
            ui.close();
        }
        ui.separator();
        let pinned: Vec<NoteId> = targets
            .iter()
            .filter(|id| {
                context
                    .board
                    .note(id)
                    .is_some_and(|note| note.layout == crate::model::LayoutMode::Pinned)
            })
            .cloned()
            .collect();
        if !pinned.is_empty() && ui.button("Unpin (back to the row)").clicked() {
            output
                .commands
                .extend(pinned.into_iter().map(|id| Command::Unpin { id }));
            ui.close();
        }
        if !protected
            && ui
                .button("Group into a container")
                .on_hover_text("Ctrl+G")
                .clicked()
        {
            self.selected = targets.to_vec();
            self.group_selection(context.board, output);
            ui.close();
        }
        let has_children = targets.iter().any(|id| {
            context.board.has_children(id) && !context.board.note(id).is_some_and(|n| n.inbox)
        });
        if has_children && ui.button("Ungroup").on_hover_text("Ctrl+Shift+G").clicked() {
            output
                .commands
                .extend(ungroup_commands(context.board, targets));
            ui.close();
        }
        if protected {
            return;
        }
        ui.separator();
        let label = if many {
            format!("Delete {} notes", targets.len())
        } else {
            "Delete".to_owned()
        };
        if ui
            .button(RichText::new(label).color(theme::SAVE_ERROR))
            .on_hover_text("Delete or Backspace")
            .clicked()
        {
            self.selected = targets.to_vec();
            self.selected_edges.clear();
            let (commands, notes) = self.delete_selection(context.board);
            output.deleted_notes += notes;
            output.commands.extend(commands);
            ui.close();
        }
    }

    fn paint_marquee(&self, ui: &mut Ui, canvas_rect: Rect) {
        let Some(marquee) = &self.marquee else {
            return;
        };
        let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let rect = Rect::from_two_pos(marquee.origin, pointer);
        let painter = ui.painter_at(canvas_rect);
        painter.rect_filled(rect, 2, theme::ACCENT.gamma_multiply(0.12));
        painter.rect_stroke(rect, 2, Stroke::new(1.0, theme::ACCENT), StrokeKind::Inside);
    }

    fn paint_drop_hint(&self, ui: &mut Ui, geometry: &Geometry, canvas_rect: Rect) {
        let Some(drag) = &self.drag else {
            return;
        };
        if !drag.moved {
            return;
        }
        let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let painter = ui.painter_at(canvas_rect);
        match &self.drop_plan {
            Some(DropPlan::Nest { target }) => {
                if let Some(rect) = geometry.screen_rect(target, &self.camera, canvas_rect) {
                    painter.rect_stroke(
                        rect.expand(3.0),
                        theme::CARD_CORNER,
                        Stroke::new(2.0, theme::ACCENT),
                        StrokeKind::Outside,
                    );
                }
            }
            Some(DropPlan::Place { marker, after, .. }) => {
                if let Some(rect) = geometry.screen_rect(marker, &self.camera, canvas_rect) {
                    let x = if *after {
                        rect.right() + 4.0
                    } else {
                        rect.left() - 4.0
                    };
                    painter.vline(x, rect.y_range(), Stroke::new(3.0, theme::ACCENT));
                }
            }
            Some(DropPlan::Pin { .. }) | None => {}
        }
        if let Some(source) = geometry.screen_rect(&drag.id, &self.camera, canvas_rect) {
            let grab = drag.grab * self.camera.zoom;
            painter.rect_stroke(
                Rect::from_min_size(pointer - grab, source.size()),
                theme::CARD_CORNER,
                Stroke::new(1.0, theme::ACCENT),
                StrokeKind::Outside,
            );
        }
    }

    /// While linking, draw a live arrow from the source to the pointer and
    /// outline the note it would link to.
    fn paint_link_hint(
        &self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        geometry: &Geometry,
        canvas_rect: Rect,
    ) {
        let Some(link) = &self.link else {
            return;
        };
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        ui.ctx().request_repaint();
        let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) else {
            return;
        };
        let Some(source) = geometry.screen_rect(&link.from, &self.camera, canvas_rect) else {
            return;
        };
        let painter = ui.painter_at(canvas_rect);
        let target = self
            .hit_note(geometry, context, pointer, canvas_rect)
            .filter(|target| *target != link.from);
        let end = match target
            .as_ref()
            .and_then(|target| geometry.screen_rect(target, &self.camera, canvas_rect))
        {
            Some(rect) => {
                painter.rect_stroke(
                    rect.expand(3.0),
                    theme::CARD_CORNER,
                    Stroke::new(2.0, theme::ACCENT),
                    StrokeKind::Outside,
                );
                border_point(rect, link_handle_center(source))
            }
            None => pointer,
        };
        let start = link_handle_center(source);
        painter.line_segment([start, end], Stroke::new(2.0, theme::ACCENT));
        paint_arrowhead(&painter, end, start, theme::ACCENT);
    }
}

/// Build the numeric-id mapping and layout for one frame.
struct Geometry {
    layout: Layout,
    note_by_num: HashMap<u64, NoteId>,
    num_by_note: HashMap<NoteId, u64>,
}

impl Geometry {
    fn build(board: &Board, now_utc_ms: i64, hide_done: bool) -> Self {
        let hidden = if hide_done {
            hidden_done_notes(board)
        } else {
            std::collections::HashSet::new()
        };
        let num_by_note: HashMap<NoteId, u64> = board
            .notes
            .iter()
            .enumerate()
            .map(|(index, note)| (note.id.clone(), index as u64))
            .collect();
        let note_by_num: HashMap<u64, NoteId> = num_by_note
            .iter()
            .map(|(id, number)| (*number, id.clone()))
            .collect();

        let notes: Vec<LayoutNote> = board
            .notes
            .iter()
            .map(|note| LayoutNote {
                id: num_by_note[&note.id],
                parent: note
                    .parent
                    .as_ref()
                    .and_then(|parent| num_by_note.get(parent).copied()),
                important: note.important,
                done: note.status == Status::Done,
                hidden: hidden.contains(&note.id),
                snoozed: is_snoozed(note, now_utc_ms),
                pinned: note.layout == crate::model::LayoutMode::Pinned,
                x: note.pos.map(|pos| pos.x).unwrap_or(0.0),
                y: note.pos.map(|pos| pos.y).unwrap_or(0.0),
                sort: note.sort,
            })
            .collect();
        let layout = layout::compute(&notes);

        Self {
            layout,
            note_by_num,
            num_by_note,
        }
    }

    fn num(&self, id: &str) -> Option<u64> {
        self.num_by_note.get(id).copied()
    }

    fn world_rect(&self, id: &str) -> Option<Rect> {
        self.layout
            .rects
            .get(&self.num(id)?)
            .map(|rect| world_rect(*rect))
    }

    fn world_rects(&self) -> HashMap<NoteId, Rect> {
        self.layout
            .rects
            .iter()
            .filter_map(|(number, rect)| {
                self.note_by_num
                    .get(number)
                    .map(|id| (id.clone(), world_rect(*rect)))
            })
            .collect()
    }

    fn screen_rect(&self, id: &str, camera: &Camera, canvas: Rect) -> Option<Rect> {
        self.world_rect(id)
            .map(|rect| screen_rect(rect, camera, canvas))
    }

    fn is_container(&self, id: &str) -> bool {
        self.num(id)
            .is_some_and(|number| self.layout.containers.contains(&number))
    }
}

/// Siblings in the order the layout shows them: open before done, important
/// first, then sort order and id.
fn display_children<'a>(board: &'a Board, parent: Option<&str>) -> Vec<&'a Note> {
    let mut children = board.children(parent);
    children.sort_by_key(|note| {
        (
            note.status == Status::Done,
            !note.important,
            note.sort,
            note.id.clone(),
        )
    });
    children
}

/// Decide where a drag of `dragged` lands when released at `world`.
///
/// * over a leaf's left or right quarter: insert before or after it;
/// * over the rest of a note (or a container's own frame): nest inside it;
/// * over empty canvas: pin at the root.
fn plan_drop(
    board: &Board,
    geometry: &Geometry,
    dragged: &[NoteId],
    world: Pos2,
    grab: Vec2,
) -> DropPlan {
    let pin = DropPlan::Pin {
        x: world.x - grab.x,
        y: world.y - grab.y,
    };
    let rects = geometry.world_rects();
    let mut candidates: Vec<(&NoteId, &Rect)> = rects
        .iter()
        .filter(|(id, rect)| {
            rect.contains(world)
                && !dragged
                    .iter()
                    .any(|moved| is_ancestor_or_self(board, moved, id))
        })
        .collect();
    candidates.sort_by(|left, right| {
        let left_area = left.1.width() * left.1.height();
        let right_area = right.1.width() * right.1.height();
        left_area
            .partial_cmp(&right_area)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let Some((target, rect)) = candidates.first().map(|(id, rect)| ((*id).clone(), **rect)) else {
        return pin;
    };
    let Some(note) = board.note(&target) else {
        return pin;
    };
    let pinned_root = note.parent.is_none() && note.layout == crate::model::LayoutMode::Pinned;
    let edge_zone = !geometry.is_container(&target) && !note.inbox && !pinned_root;
    if edge_zone {
        let fraction = (world.x - rect.left()) / rect.width().max(1.0);
        if fraction < 0.25 || fraction > 0.75 {
            let after = fraction > 0.75;
            let before = if after {
                let siblings: Vec<&Note> = display_children(board, note.parent.as_deref())
                    .into_iter()
                    .filter(|sibling| !dragged.contains(&sibling.id))
                    .collect();
                siblings
                    .iter()
                    .position(|sibling| sibling.id == target)
                    .and_then(|index| siblings.get(index + 1))
                    .map(|sibling| sibling.id.clone())
            } else {
                Some(target.clone())
            };
            return DropPlan::Place {
                parent: note.parent.clone(),
                before,
                marker: target,
                after,
            };
        }
    }
    DropPlan::Nest { target }
}

/// The commands that carry out a drop plan for the dragged notes.
fn drop_commands(
    board: &Board,
    geometry: &Geometry,
    dragged: &[NoteId],
    plan: &DropPlan,
) -> Vec<Command> {
    let mut commands = Vec::new();
    match plan {
        DropPlan::Nest { target } => {
            for id in dragged {
                if board.note(id).is_some_and(|note| !note.inbox) {
                    commands.push(Command::SetParent {
                        id: id.clone(),
                        parent: Some(target.clone()),
                        before: None,
                    });
                }
            }
        }
        DropPlan::Place { parent, before, .. } => {
            for id in dragged {
                let Some(note) = board.note(id) else {
                    continue;
                };
                if note.inbox && parent.is_some() {
                    continue;
                }
                if note.parent.is_none()
                    && parent.is_none()
                    && note.layout == crate::model::LayoutMode::Pinned
                {
                    commands.push(Command::Unpin { id: id.clone() });
                }
                commands.push(Command::SetParent {
                    id: id.clone(),
                    parent: parent.clone(),
                    before: before.clone(),
                });
            }
        }
        DropPlan::Pin { x, y } => {
            let anchor = dragged.first().and_then(|id| geometry.world_rect(id));
            for id in dragged {
                let Some(note) = board.note(id) else {
                    continue;
                };
                let offset = match (anchor, geometry.world_rect(id)) {
                    (Some(anchor), Some(rect)) => rect.min - anchor.min,
                    _ => Vec2::ZERO,
                };
                if note.parent.is_some() {
                    commands.push(Command::SetParent {
                        id: id.clone(),
                        parent: None,
                        before: None,
                    });
                }
                commands.push(Command::Pin {
                    id: id.clone(),
                    x: x + offset.x,
                    y: y + offset.y,
                });
            }
        }
    }
    commands
}

/// Done notes with no open descendants: the set "Hide done" removes.
fn hidden_done_notes(board: &Board) -> std::collections::HashSet<NoteId> {
    let mut children: HashMap<&str, Vec<&Note>> = HashMap::new();
    for note in &board.notes {
        if let Some(parent) = &note.parent {
            children.entry(parent.as_str()).or_default().push(note);
        }
    }
    fn has_open(note: &Note, children: &HashMap<&str, Vec<&Note>>, depth: usize) -> bool {
        if depth > 64 {
            return true;
        }
        note.status != Status::Done
            || children
                .get(note.id.as_str())
                .is_some_and(|kids| kids.iter().any(|kid| has_open(kid, children, depth + 1)))
    }
    board
        .notes
        .iter()
        .filter(|note| !has_open(note, &children, 0))
        .map(|note| note.id.clone())
        .collect()
}

/// True when candidate is the note itself or one of its descendants, which
/// would create a cycle on drop.
fn is_ancestor_or_self(board: &Board, note: &str, candidate: &str) -> bool {
    if note == candidate {
        return true;
    }
    let mut current = board.note(candidate).and_then(|note| note.parent.clone());
    let mut guard = 0;
    while let Some(parent) = current {
        if parent == note {
            return true;
        }
        guard += 1;
        if guard > 64 {
            return true;
        }
        current = board.note(&parent).and_then(|note| note.parent.clone());
    }
    false
}

fn world_rect(rect: layout::Rect) -> Rect {
    Rect::from_min_size(pos2(rect.x, rect.y), vec2(rect.w, rect.h))
}

fn screen_rect(rect: Rect, camera: &Camera, canvas: Rect) -> Rect {
    Rect::from_min_max(
        camera.world_to_screen(rect.min, canvas),
        camera.world_to_screen(rect.max, canvas),
    )
}

fn paint_grid(painter: &egui::Painter, camera: &Camera, canvas_rect: Rect) {
    let step = camera.grid_step();
    if !step.is_finite() || step <= 0.0 {
        return;
    }
    let world_min = camera.screen_to_world(canvas_rect.left_top(), canvas_rect);
    let world_max = camera.screen_to_world(canvas_rect.right_bottom(), canvas_rect);
    let minor = Stroke::new(1.0, theme::GRID_MINOR);
    let major = Stroke::new(1.0, theme::GRID_MAJOR);

    let first_x = (world_min.x / step).floor() as i64;
    let last_x = (world_max.x / step).ceil() as i64;
    let count_x = last_x.saturating_sub(first_x);
    if count_x <= 4096 {
        for offset in 0..=count_x {
            let index = first_x.saturating_add(offset);
            let screen_x = camera
                .world_to_screen(pos2(index as f32 * step, 0.0), canvas_rect)
                .x;
            painter.vline(
                screen_x,
                canvas_rect.y_range(),
                if index.rem_euclid(5) == 0 {
                    major
                } else {
                    minor
                },
            );
        }
    }

    let first_y = (world_min.y / step).floor() as i64;
    let last_y = (world_max.y / step).ceil() as i64;
    let count_y = last_y.saturating_sub(first_y);
    if count_y <= 4096 {
        for offset in 0..=count_y {
            let index = first_y.saturating_add(offset);
            let screen_y = camera
                .world_to_screen(pos2(0.0, index as f32 * step), canvas_rect)
                .y;
            painter.hline(
                canvas_rect.x_range(),
                screen_y,
                if index.rem_euclid(5) == 0 {
                    major
                } else {
                    minor
                },
            );
        }
    }

    let origin = camera.world_to_screen(pos2(0.0, 0.0), canvas_rect);
    let axis = Stroke::new(1.0, theme::GRID_AXIS);
    if canvas_rect.left() <= origin.x && origin.x <= canvas_rect.right() {
        painter.vline(origin.x, canvas_rect.y_range(), axis);
    }
    if canvas_rect.top() <= origin.y && origin.y <= canvas_rect.bottom() {
        painter.hline(canvas_rect.x_range(), origin.y, axis);
    }
}

fn paint_edges(
    painter: &egui::Painter,
    board: &Board,
    geometry: &Geometry,
    camera: &Camera,
    canvas_rect: Rect,
    selected: &[EdgeId],
    hovered: Option<&str>,
) {
    for edge in &board.edges {
        let Some(from) = geometry.num(&edge.from) else {
            continue;
        };
        let Some(to) = geometry.num(&edge.to) else {
            continue;
        };
        let Some(bezier) = layout::edge_geometry(from, to, &geometry.layout.rects) else {
            continue;
        };
        let points = edge_screen_points(&bezier, camera, canvas_rect);
        let is_selected = selected.contains(&edge.id);
        let is_hovered = hovered == Some(edge.id.as_str());
        let color = if is_selected {
            theme::ACCENT
        } else if is_hovered {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        };
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [points[0], points[1], points[2], points[3]],
            false,
            Color32::TRANSPARENT,
            Stroke::new(if is_selected || is_hovered { 2.5 } else { 1.5 }, color),
        ));
        paint_arrowhead(painter, points[3], points[2], color);
    }
}

fn edge_screen_points(geometry: &EdgeGeometry, camera: &Camera, canvas: Rect) -> [Pos2; 4] {
    [
        camera.world_to_screen(pos2(geometry.start.x, geometry.start.y), canvas),
        camera.world_to_screen(pos2(geometry.control1.x, geometry.control1.y), canvas),
        camera.world_to_screen(pos2(geometry.control2.x, geometry.control2.y), canvas),
        camera.world_to_screen(pos2(geometry.end.x, geometry.end.y), canvas),
    ]
}

fn paint_arrowhead(painter: &egui::Painter, tip: Pos2, from: Pos2, color: Color32) {
    let direction = (tip - from).normalized();
    if !direction.is_finite() {
        return;
    }
    let normal = vec2(-direction.y, direction.x);
    let back = tip - direction * 10.0;
    painter.add(egui::Shape::convex_polygon(
        vec![tip, back + normal * 5.0, back - normal * 5.0],
        color,
        Stroke::NONE,
    ));
}

fn hit_edge(
    board: &Board,
    geometry: &Geometry,
    camera: &Camera,
    canvas_rect: Rect,
    pointer: Pos2,
) -> Option<EdgeId> {
    let mut best: Option<(f32, EdgeId)> = None;
    for edge in &board.edges {
        let Some(from) = geometry.num(&edge.from) else {
            continue;
        };
        let Some(to) = geometry.num(&edge.to) else {
            continue;
        };
        let Some(bezier) = layout::edge_geometry(from, to, &geometry.layout.rects) else {
            continue;
        };
        let points = edge_screen_points(&bezier, camera, canvas_rect);
        let distance = polyline_distance(&points, pointer);
        if distance <= EDGE_HIT_RADIUS && best.as_ref().is_none_or(|(best, _)| distance < *best) {
            best = Some((distance, edge.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

fn polyline_distance(points: &[Pos2; 4], point: Pos2) -> f32 {
    let mut best = f32::MAX;
    let mut previous = points[0];
    for step in 1..=32 {
        let current = cubic_point(points, step as f32 / 32.0);
        best = best.min(segment_distance(previous, current, point));
        previous = current;
    }
    best
}

fn cubic_point(points: &[Pos2; 4], t: f32) -> Pos2 {
    let inverse = 1.0 - t;
    let a = inverse * inverse * inverse;
    let b = 3.0 * inverse * inverse * t;
    let c = 3.0 * inverse * t * t;
    let d = t * t * t;
    pos2(
        a * points[0].x + b * points[1].x + c * points[2].x + d * points[3].x,
        a * points[0].y + b * points[1].y + c * points[2].y + d * points[3].y,
    )
}

fn segment_distance(a: Pos2, b: Pos2, point: Pos2) -> f32 {
    let ab = b - a;
    let length_squared = ab.length_sq();
    if length_squared <= f32::EPSILON {
        return (point - a).length();
    }
    let t = ((point - a).dot(ab) / length_squared).clamp(0.0, 1.0);
    (point - (a + ab * t)).length()
}

fn paint_card(
    painter: &egui::Painter,
    card: Rect,
    header: Rect,
    corner: u8,
    palette: theme::NotePalette,
    selected: bool,
    opacity: f32,
) {
    let shadow = Rect::from_min_size(card.min + vec2(0.0, 4.0), card.size());
    painter.rect_filled(shadow, corner, dim(theme::NOTE_SHADOW, opacity));
    painter.rect_filled(card, corner, dim(palette.fill, opacity));
    painter.rect_filled(header, corner, dim(palette.header, opacity));
    painter.hline(
        card.x_range(),
        header.bottom(),
        Stroke::new(1.0, dim(theme::NOTE_EDGE, opacity)),
    );
    let stroke = if selected {
        Stroke::new(2.0, dim(theme::ACCENT, opacity.max(0.6)))
    } else {
        Stroke::new(1.0, dim(theme::NOTE_EDGE, opacity))
    };
    painter.rect_stroke(card, corner, stroke, StrokeKind::Inside);
}

fn paint_due_chip(painter: &egui::Painter, card: Rect, note: &Note, zoom: f32, opacity: f32) {
    let Some(due) = &note.due else {
        return;
    };
    let label = format_due(due);
    let font_size = (11.0 * zoom).clamp(4.0, 14.0);
    let padding = (theme::NOTE_PADDING * zoom).max(2.0);
    painter.text(
        pos2(card.left() + padding, card.bottom() - padding),
        Align2::LEFT_BOTTOM,
        label,
        FontId::proportional(font_size),
        dim(theme::NOTE_INK, opacity * 0.8),
    );
}

fn paint_snooze_mark(painter: &egui::Painter, card: Rect, zoom: f32, opacity: f32) {
    let font_size = (12.0 * zoom).clamp(4.0, 14.0);
    painter.text(
        pos2(card.right() - 8.0 * zoom, card.bottom() - 8.0 * zoom),
        Align2::RIGHT_BOTTOM,
        "z",
        FontId::proportional(font_size),
        dim(theme::TEXT_MUTED, opacity),
    );
}

/// The title strip at the top of every card.
fn header_rect(screen: Rect, zoom: f32) -> Rect {
    let height = (CONTAINER_HEADER * zoom).clamp(10.0, (screen.height() * 0.5).max(10.0));
    Rect::from_min_size(screen.min, vec2(screen.width(), height))
}

/// The importance slot ("!") at the right of a header.
fn important_rect(header: Rect, zoom: f32) -> Rect {
    let side = (CHECKBOX_SIZE * zoom).clamp(6.0, header.height() * 0.8);
    let padding = (theme::NOTE_PADDING * zoom).max(2.0);
    Rect::from_min_size(
        pos2(
            header.right() - padding - side,
            header.center().y - side * 0.5,
        ),
        vec2(side, side),
    )
}

/// The done checkbox at the left of a header.
fn checkbox_rect(header: Rect, zoom: f32) -> Rect {
    let side = (CHECKBOX_SIZE * zoom).clamp(6.0, header.height() * 0.8);
    let padding = (theme::NOTE_PADDING * zoom).max(2.0);
    Rect::from_min_size(
        pos2(header.left() + padding, header.center().y - side * 0.5),
        vec2(side, side),
    )
}

fn paint_checkbox(painter: &egui::Painter, rect: Rect, done: bool, ink: Color32, opacity: f32) {
    let corner = (rect.width() * 0.2) as u8;
    if done {
        painter.rect_filled(rect, corner, dim(theme::ACCENT, opacity));
        let width = rect.width();
        let points = vec![
            rect.left_top() + vec2(width * 0.22, width * 0.52),
            rect.left_top() + vec2(width * 0.42, width * 0.72),
            rect.left_top() + vec2(width * 0.78, width * 0.30),
        ];
        painter.add(egui::Shape::line(
            points,
            Stroke::new((width * 0.14).max(1.0), dim(theme::CANVAS_BG, opacity)),
        ));
    } else {
        painter.rect_stroke(
            rect,
            corner,
            Stroke::new((rect.width() * 0.09).max(1.0), dim(ink, opacity * 0.7)),
            StrokeKind::Inside,
        );
    }
}

/// (done, total) over a container's direct children.
fn child_progress(board: &Board, id: &str) -> Option<(usize, usize)> {
    let children = board
        .notes
        .iter()
        .filter(|note| note.parent.as_deref() == Some(id));
    let (done, total) = children.fold((0, 0), |(done, total), note| {
        (done + usize::from(note.status == Status::Done), total + 1)
    });
    (total > 0).then_some((done, total))
}

/// Ctrl+Shift+G: move each selected container's children to its parent, in
/// its place, then delete the empty container. The Inbox is never ungrouped.
fn ungroup_commands(board: &Board, targets: &[NoteId]) -> Vec<Command> {
    let mut commands = Vec::new();
    for id in targets {
        let Some(container) = board.note(id) else {
            continue;
        };
        if container.inbox {
            continue;
        }
        let children = display_children(board, Some(id));
        if children.is_empty() {
            continue;
        }
        if container.parent.is_none() && container.layout == crate::model::LayoutMode::Pinned {
            commands.push(Command::Unpin { id: id.clone() });
        }
        for child in children {
            commands.push(Command::SetParent {
                id: child.id.clone(),
                parent: container.parent.clone(),
                before: Some(id.clone()),
            });
        }
        commands.push(Command::Delete { id: id.clone() });
    }
    commands
}

/// Mark every target important, or clear the flag when all already have it.
fn toggle_important_commands(board: &Board, targets: &[NoteId]) -> Vec<Command> {
    let notes: Vec<&Note> = targets.iter().filter_map(|id| board.note(id)).collect();
    let important = !notes.iter().all(|note| note.important);
    notes
        .iter()
        .filter(|note| note.important != important)
        .map(|note| Command::SetImportant {
            id: note.id.clone(),
            important,
        })
        .collect()
}

/// Complete every open target, or reopen them all when every target is done.
fn toggle_done_commands(board: &Board, targets: &[NoteId]) -> Vec<Command> {
    let notes: Vec<&Note> = targets
        .iter()
        .filter_map(|id| board.note(id))
        .filter(|note| !note.inbox)
        .collect();
    if notes.is_empty() {
        return Vec::new();
    }
    if notes.iter().all(|note| note.status == Status::Done) {
        notes
            .iter()
            .map(|note| Command::Reopen {
                id: note.id.clone(),
            })
            .collect()
    } else {
        notes
            .iter()
            .filter(|note| note.status != Status::Done)
            .map(|note| Command::Complete {
                id: note.id.clone(),
            })
            .collect()
    }
}

/// Centre of the link handle: on the note's right border, half outside the
/// card so it never covers the text.
fn link_handle_center(screen: Rect) -> Pos2 {
    pos2(screen.right(), screen.center().y)
}

/// Where the line from `rect`'s centre towards `from` crosses its border.
fn border_point(rect: Rect, from: Pos2) -> Pos2 {
    let center = rect.center();
    let direction = from - center;
    if direction.length_sq() <= f32::EPSILON {
        return center;
    }
    let scale_x = if direction.x.abs() > f32::EPSILON {
        (rect.width() * 0.5) / direction.x.abs()
    } else {
        f32::INFINITY
    };
    let scale_y = if direction.y.abs() > f32::EPSILON {
        (rect.height() * 0.5) / direction.y.abs()
    } else {
        f32::INFINITY
    };
    center + direction * scale_x.min(scale_y).min(1.0)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn format_due(due: &TimeSpec) -> String {
    let local = due.local.as_str();
    match (local.get(5..7), local.get(8..10), local.get(11..16)) {
        (Some(month), Some(day), Some(time)) => format!("due {month}-{day} {time}"),
        _ => format!("due {local}"),
    }
}

/// Opacity for a note: only an active snooze dims it.
pub fn opacity_for(note: &Note, now_utc_ms: i64) -> f32 {
    if is_snoozed(note, now_utc_ms) {
        SNOOZE_OPACITY
    } else {
        1.0
    }
}

fn is_snoozed(note: &Note, now_utc_ms: i64) -> bool {
    note.snooze
        .as_ref()
        .and_then(|snooze| snooze.resolve().ok())
        .is_some_and(|resolved| resolved.utc_ms > now_utc_ms)
}

fn dim(color: Color32, opacity: f32) -> Color32 {
    color.gamma_multiply(opacity.clamp(0.0, 1.0))
}

fn snooze_after(now_utc_ms: i64, minutes: i64, tz: &str) -> Option<TimeSpec> {
    let target = now_utc_ms.checked_add(minutes.checked_mul(60_000)?)?;
    time_spec_at(target, tz)
}

fn snooze_tomorrow(now_utc_ms: i64, tz: &str) -> Option<TimeSpec> {
    let (_, end) = crate::time::today_bounds(now_utc_ms, tz).ok()?;
    time_spec_at(end.checked_add(9 * 60 * 60 * 1000)?, tz)
}

fn time_spec_at(utc_ms: i64, tz: &str) -> Option<TimeSpec> {
    let zone = jiff::tz::TimeZone::get(tz).ok()?;
    let timestamp = jiff::Timestamp::from_millisecond(utc_ms).ok()?;
    let local = zone.to_datetime(timestamp);
    Some(TimeSpec {
        local: format!("{local:.0}"),
        tz: tz.to_owned(),
    })
}

/// True when the pointer is over the board itself, not over a floating panel,
/// popup or menu drawn above it.
fn on_board_layer(ui: &Ui, pointer: Pos2) -> bool {
    ui.ctx()
        .layer_id_at(pointer)
        .is_none_or(|layer| layer == ui.layer_id())
}

fn surrender_focus(ctx: &egui::Context) {
    if let Some(focused) = ctx.memory(|memory| memory.focused()) {
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StorePaths, schema, scratch_dir};

    const TZ: &str = "Europe/London";

    fn store(name: &str) -> Store {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("board opens")
    }

    fn board_and_root(store: &mut Store) -> (Board, NoteId) {
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: "parent".to_owned(),
                    parent: None,
                    container: None,
                    at_root: true,
                    due: None,
                    remind: None,
                    important: false,
                    source: "test".to_owned(),
                }],
                crate::command::Actor::Human,
                None,
            )
            .expect("add");
        let id = applied.new_ids.notes[0].clone();
        (Board::load(store.connection()).expect("load"), id)
    }

    fn add_note(store: &mut Store, text: &str, parent: Option<&str>) -> NoteId {
        store
            .apply(
                &[Command::AddNote {
                    text: text.to_owned(),
                    parent: parent.map(str::to_owned),
                    container: None,
                    at_root: true,
                    due: None,
                    remind: None,
                    important: false,
                    source: "test".to_owned(),
                }],
                crate::command::Actor::Human,
                None,
            )
            .expect("add note")
            .new_ids
            .notes[0]
            .clone()
    }

    #[test]
    fn geometry_hides_done_leaves_and_marks_containers() {
        let mut store = store("board-geometry");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 1_800_000_000_000, true);
        assert!(
            geometry.is_container(&root),
            "a note with children is a container"
        );
        assert!(geometry.world_rect(&child).is_some());

        store
            .apply(
                &[Command::Complete { id: child.clone() }],
                crate::command::Actor::Human,
                None,
            )
            .expect("complete");
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 1_800_000_000_000, true);
        assert!(
            geometry.world_rect(&child).is_none(),
            "a done leaf has no rect when done notes are hidden"
        );
    }

    #[test]
    fn root_drop_pins_at_the_pointer() {
        let mut store = store("board-pin");
        let (board, root) = board_and_root(&mut store);
        let geometry = Geometry::build(&board, 0, false);
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: root.clone(),
            grab: vec2(10.0, 5.0),
            moved: true,
            reselect: false,
        });
        let commands = view.commit_note_drop(&board, &geometry, pos2(300.0, 200.0));
        assert!(matches!(
            commands.as_slice(),
            [Command::Pin { id, x, y }]
                if id == &root && (*x - 290.0).abs() < 0.01 && (*y - 195.0).abs() < 0.01
        ));
    }

    #[test]
    fn child_drop_reparents_to_the_container_under_the_pointer() {
        let mut store = store("board-reparent");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
            reselect: false,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&root].center());
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent: Some(target), .. }] if id == &child && target == &root
        ));
    }

    #[test]
    fn child_drop_on_itself_unparents() {
        let mut store = store("board-self-drop");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
            reselect: false,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&child].center());
        // The parent container is also under the pointer, so the result may be
        // the current parent, but never the note itself.
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent, .. }]
                if id == &child && parent.as_deref() != Some(child.as_str())
        ));
    }

    #[test]
    fn child_drop_on_a_descendant_is_never_the_target() {
        let mut store = store("board-descendant-drop");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let grandchild = add_note(&mut store, "grandchild", Some(&child));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
            reselect: false,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&grandchild].center());
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent, .. }]
                if id == &child
                    && parent.as_deref() != Some(child.as_str())
                    && parent.as_deref() != Some(grandchild.as_str())
        ));
    }

    #[test]
    fn snoozed_notes_are_dimmed_until_the_snooze_expires() {
        let mut store = store("board-opacity");
        let (board, root) = board_and_root(&mut store);
        let note = board.note(&root).expect("note").clone();
        assert_eq!(opacity_for(&note, 0), 1.0);

        store
            .apply(
                &[Command::Snooze {
                    id: note.id.clone(),
                    until: TimeSpec {
                        local: "2030-01-01T09:00:00".to_owned(),
                        tz: TZ.to_owned(),
                    },
                }],
                crate::command::Actor::Human,
                None,
            )
            .expect("snooze");
        let board = Board::load(store.connection()).expect("load");
        let snoozed = board.note(&root).expect("note");
        assert!((opacity_for(snoozed, 1_800_000_000_000) - SNOOZE_OPACITY).abs() < 0.001);
        assert_eq!(opacity_for(snoozed, 1_900_000_000_000), 1.0);
    }

    #[test]
    fn arrow_hit_test_finds_the_edge_between_two_notes() {
        let mut store = store("board-edge-hit");
        let (_, first) = board_and_root(&mut store);
        let second = add_note(&mut store, "second", None);
        let edge = store
            .apply(
                &[Command::Link {
                    from: first,
                    to: second,
                    kind: None,
                }],
                crate::command::Actor::Human,
                None,
            )
            .expect("link")
            .new_ids
            .edges[0]
            .clone();
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let camera = Camera::default();
        let canvas = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 800.0));
        let edge_note = board.edge(&edge).expect("edge");
        let bezier = layout::edge_geometry(
            geometry.num(&edge_note.from).expect("from"),
            geometry.num(&edge_note.to).expect("to"),
            &geometry.layout.rects,
        )
        .expect("geometry");
        let points = edge_screen_points(&bezier, &camera, canvas);
        assert_eq!(
            hit_edge(
                &board,
                &geometry,
                &camera,
                canvas,
                cubic_point(&points, 0.5)
            ),
            Some(edge.clone())
        );
        assert_eq!(
            hit_edge(&board, &geometry, &camera, canvas, pos2(5.0, 5.0)),
            None
        );

        let mut view = BoardView::new();
        view.selected_edges = vec![edge.clone()];
        let (commands, notes) = view.delete_selection(&board);
        assert_eq!(notes, 0);
        assert!(matches!(
            commands.as_slice(),
            [Command::Unlink { edge_id }] if *edge_id == edge
        ));
        assert!(view.delete_selection(&board).0.is_empty());
    }

    #[test]
    fn deleting_a_container_and_its_child_emits_one_delete() {
        let mut store = store("board-delete-subtree");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let other = add_note(&mut store, "other", None);
        let board = Board::load(store.connection()).expect("load");
        let mut view = BoardView::new();
        view.selected = vec![child.clone(), root.clone(), other.clone()];
        let (commands, notes) = view.delete_selection(&board);
        assert_eq!(notes, 2);
        assert!(matches!(
            commands.as_slice(),
            [Command::Delete { id: first }, Command::Delete { id: second }]
                if *first == root && *second == other
        ));
        assert!(view.selected.is_empty());
        store
            .apply(&commands, crate::command::Actor::Human, None)
            .expect("the batch applies without touching the child twice");
        let board = Board::load(store.connection()).expect("load");
        assert!(board.notes.is_empty());
    }

    #[test]
    fn toggle_done_completes_open_targets_or_reopens_all_done_ones() {
        let mut store = store("board-toggle-done");
        let (_, first) = board_and_root(&mut store);
        let second = add_note(&mut store, "second", Some(&first));
        store
            .apply(
                &[Command::Complete { id: second.clone() }],
                crate::command::Actor::Human,
                None,
            )
            .expect("complete");
        let board = Board::load(store.connection()).expect("load");
        let mixed = toggle_done_commands(&board, &[first.clone(), second.clone()]);
        assert!(matches!(mixed.as_slice(), [Command::Complete { id }] if *id == first));
        let done = toggle_done_commands(&board, std::slice::from_ref(&second));
        assert!(matches!(done.as_slice(), [Command::Reopen { id }] if *id == second));
        assert_eq!(child_progress(&board, &first), Some((1, 1)));
        assert_eq!(child_progress(&board, &second), None);
    }

    #[test]
    fn hide_done_keeps_a_done_container_with_open_children() {
        let mut store = store("board-hide-done");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        store
            .apply(
                &[Command::Complete { id: root.clone() }],
                crate::command::Actor::Human,
                None,
            )
            .expect("complete");
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, true);
        assert!(geometry.world_rect(&root).is_some(), "it has an open child");
        assert!(geometry.world_rect(&child).is_some());
        let shown = Geometry::build(&board, 0, false);
        assert!(shown.world_rect(&root).is_some());
    }

    #[test]
    fn drop_zones_reorder_at_the_edges_and_nest_in_the_middle() {
        let mut store = store("board-drop-zones");
        let (_, first) = board_and_root(&mut store);
        let second = add_note(&mut store, "second", None);
        let third = add_note(&mut store, "third", None);
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rects = geometry.world_rects();
        let target = rects[&first];
        let dragged = vec![third.clone()];

        let left = pos2(target.left() + target.width() * 0.1, target.center().y);
        assert_eq!(
            plan_drop(&board, &geometry, &dragged, left, Vec2::ZERO),
            DropPlan::Place {
                parent: None,
                before: Some(first.clone()),
                marker: first.clone(),
                after: false,
            }
        );
        let right = pos2(target.left() + target.width() * 0.9, target.center().y);
        assert!(matches!(
            plan_drop(&board, &geometry, &dragged, right, Vec2::ZERO),
            DropPlan::Place { before: Some(ref before), after: true, .. } if *before == second
        ));
        assert_eq!(
            plan_drop(&board, &geometry, &dragged, target.center(), Vec2::ZERO),
            DropPlan::Nest {
                target: first.clone()
            }
        );
        assert!(matches!(
            plan_drop(
                &board,
                &geometry,
                &dragged,
                pos2(-500.0, -500.0),
                Vec2::ZERO
            ),
            DropPlan::Pin { .. }
        ));
    }

    #[test]
    fn a_multi_selection_drag_moves_every_selected_note() {
        let mut store = store("board-multi-drag");
        let (_, target) = board_and_root(&mut store);
        let a = add_note(&mut store, "a", None);
        let b = add_note(&mut store, "b", None);
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.selected = vec![a.clone(), b.clone()];
        view.drag = Some(NoteDrag {
            id: a.clone(),
            grab: Vec2::ZERO,
            moved: true,
            reselect: false,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&target].center());
        let moved: Vec<&str> = commands
            .iter()
            .filter_map(|command| match command {
                Command::SetParent {
                    id,
                    parent: Some(parent),
                    ..
                } if *parent == target => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(moved, vec![a.as_str(), b.as_str()]);
    }

    #[test]
    fn ungroup_lifts_children_into_the_containers_place_and_deletes_it() {
        let mut store = store("board-ungroup");
        let (_, container) = board_and_root(&mut store);
        let kid = add_note(&mut store, "kid", Some(&container));
        let board = Board::load(store.connection()).expect("load");
        let commands = ungroup_commands(&board, std::slice::from_ref(&container));
        assert!(matches!(
            commands.as_slice(),
            [
                Command::SetParent { id, parent: None, before: Some(before) },
                Command::Delete { id: deleted },
            ] if *id == kid && *before == container && *deleted == container
        ));
        store
            .apply(&commands, crate::command::Actor::Human, None)
            .expect("ungroup applies");
        let board = Board::load(store.connection()).expect("load");
        assert!(board.note(&container).is_none());
        assert_eq!(board.note(&kid).expect("kid").parent, None);
    }

    #[test]
    fn toggle_important_sets_all_or_clears_all() {
        let mut store = store("board-important");
        let (_, a) = board_and_root(&mut store);
        let b = add_note(&mut store, "b", None);
        let board = Board::load(store.connection()).expect("load");
        let set = toggle_important_commands(&board, &[a.clone(), b.clone()]);
        assert_eq!(set.len(), 2);
        store
            .apply(&set, crate::command::Actor::Human, None)
            .expect("apply");
        let board = Board::load(store.connection()).expect("load");
        let clear = toggle_important_commands(&board, &[a.clone(), b.clone()]);
        assert!(clear.iter().all(|command| matches!(
            command,
            Command::SetImportant {
                important: false,
                ..
            }
        )));
    }

    #[test]
    fn pinned_note_stays_at_its_stored_position() {
        let mut store = store("board-pinned");
        let (_, root) = board_and_root(&mut store);
        store
            .apply(
                &[Command::Pin {
                    id: root.clone(),
                    x: 123.0,
                    y: 456.0,
                }],
                crate::command::Actor::Human,
                None,
            )
            .expect("pin");
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let rect = geometry.world_rect(&root).expect("rect");
        assert!((rect.min.x - 123.0).abs() < 0.01);
        assert!((rect.min.y - 456.0).abs() < 0.01);
    }

    #[test]
    fn note_under_pointer_is_found_and_empty_space_is_not() {
        let mut store = store("board-hit");
        let (_, root) = board_and_root(&mut store);
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let view = BoardView::new();
        let canvas = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 800.0));
        let context = BoardContext {
            board: &board,
            now_utc_ms: 0,
            tz: TZ,
            hide_done: false,
        };
        let screen = geometry
            .screen_rect(&root, &view.camera, canvas)
            .expect("screen");
        assert_eq!(
            view.hit_note(&geometry, &context, screen.center(), canvas),
            Some(root)
        );
        assert_eq!(
            view.hit_note(&geometry, &context, pos2(2.0, 2.0), canvas),
            None
        );
    }

    #[test]
    fn model_and_layout_ids_map_both_ways() {
        let mut store = store("board-ids");
        let (_, root) = board_and_root(&mut store);
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0, false);
        let number = geometry.num(&root).expect("number");
        assert_eq!(geometry.note_by_num[&number], root);
        assert_eq!(geometry.num_by_note.len(), board.notes.len());
    }

    #[test]
    fn snooze_helpers_produce_valid_local_times() {
        let now = 1_800_000_000_000;
        let spec = snooze_after(now, 60, TZ).expect("snooze");
        assert_eq!(spec.tz, TZ);
        assert!(spec.local.len() == 19);
        spec.resolve().expect("resolves");
        let tomorrow = snooze_tomorrow(now, TZ).expect("tomorrow");
        tomorrow.resolve().expect("resolves");
    }
}
