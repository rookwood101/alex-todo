//! Board canvas for the SQLite model (task 100).
//!
//! The view turns [model::Board] into layout geometry, paints groups, nested
//! containers, notes and arrows, and turns pointer input into [Command] values.
//! It owns no database handle: the app applies the returned commands through
//! [crate::command::Store::apply] and reloads.
//!
//! Interaction rules from the brief:
//! * root notes pin where they are dropped;
//! * child notes reparent when dropped over a container, otherwise they move to
//!   the root;
//! * a drop on the note itself or one of its descendants is rejected;
//! * a selected note shows a link handle; drag it onto another note to link;
//! * a selected arrow is removed with Delete;
//! * done notes are not painted; snoozed notes are painted at 35% opacity;
//! * there is no delete affordance for notes here (task 170 owns the toolbar).

use std::collections::HashMap;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::camera::Camera;
use crate::command::Command;
use crate::layout::{self, EdgeGeometry, Layout, LayoutNote};
use crate::model::{Board, EdgeId, GroupId, Note, NoteId, Priority, Status};
use crate::theme;
use crate::time::TimeSpec;

/// Opacity used for a snoozed note, per the design steer.
pub const SNOOZE_OPACITY: f32 = 0.35;
/// A drop must move at least this many screen pixels to count as a drag.
const DRAG_THRESHOLD: f32 = 3.0;
/// Hit radius around an arrow, in screen pixels.
const EDGE_HIT_RADIUS: f32 = 7.0;
/// Radius of the link handle drawn on the selected note.
const LINK_HANDLE_RADIUS: f32 = 7.0;
/// Height of a container header in world units (matches the layout engine).
const CONTAINER_HEADER: f32 = 36.0;

/// Everything the view needs from the app for one frame.
pub struct BoardContext<'a> {
    pub board: &'a Board,
    pub now_utc_ms: i64,
    /// IANA zone used to format deadlines and to build snooze values.
    pub tz: &'a str,
}

/// What the frame produced. The app applies the commands in one batch.
#[derive(Clone, Debug)]
pub struct BoardOutput {
    pub commands: Vec<Command>,
    pub canvas_rect: Rect,
    /// True while a note is being edited inline (the app defers reloads).
    pub editing: bool,
}

impl Default for BoardOutput {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            canvas_rect: Rect::ZERO,
            editing: false,
        }
    }
}

/// The camera and the interaction state that survives between frames.
pub struct BoardView {
    pub camera: Camera,
    pub selected: Option<NoteId>,
    pub selected_edge: Option<EdgeId>,
    editing: Option<NoteId>,
    edit_buffer: String,
    focus_requested: Option<NoteId>,
    fit_requested: bool,
    drag: Option<NoteDrag>,
    link: Option<LinkDrag>,
    drop_target: Option<NoteId>,
    pan_button: Option<PointerButton>,
}

#[derive(Clone, Debug)]
struct NoteDrag {
    id: NoteId,
    /// Pointer offset from the note's top-left, in world units.
    grab: Vec2,
    moved: bool,
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
            selected: None,
            selected_edge: None,
            editing: None,
            edit_buffer: String::new(),
            focus_requested: None,
            fit_requested: false,
            drag: None,
            link: None,
            drop_target: None,
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

    pub fn editing_note(&self) -> Option<&NoteId> {
        self.editing.as_ref()
    }

    pub fn camera(&self) -> Camera {
        self.camera
    }

    pub fn set_camera(&mut self, camera: Camera) {
        self.camera = camera;
    }

    /// Clear transient state after a board reload.
    pub fn after_reload(&mut self, board: &Board) {
        if let Some(id) = &self.selected
            && board.note(id).is_none()
        {
            self.selected = None;
        }
        if let Some(id) = &self.selected_edge
            && board.edge(id).is_none()
        {
            self.selected_edge = None;
        }
        if let Some(id) = &self.editing
            && board.note(id).is_none()
        {
            self.editing = None;
        }
    }

    /// Start editing a note's text.
    pub fn edit_note(&mut self, board: &Board, id: NoteId) {
        self.selected = Some(id.clone());
        self.selected_edge = None;
        self.editing = Some(id.clone());
        self.edit_buffer = board
            .note(&id)
            .map(|note| note.text.clone())
            .unwrap_or_default();
        self.focus_requested = Some(id);
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

        let geometry = Geometry::build(context.board, context.now_utc_ms);

        if self.fit_requested {
            self.fit_requested = false;
            if let Some(bounds) = geometry.layout.bounds() {
                self.camera.fit_to_bounds(world_rect(bounds), canvas_rect);
            } else {
                self.camera.reset_to_origin();
            }
        }

        self.handle_wheel(ui, canvas_rect);
        self.handle_pan(ui, canvas_rect);
        self.handle_note_input(ui, context, &geometry, canvas_rect, &mut output);
        self.handle_link_input(ui, context, &geometry, canvas_rect, &mut output);
        self.handle_edge_input(ui, context, &mut output);

        let painter = ui.painter_at(canvas_rect);
        painter.rect_filled(canvas_rect, 0, theme::CANVAS_BG);
        paint_grid(&painter, &self.camera, canvas_rect);
        paint_edges(
            &painter,
            context.board,
            &geometry,
            &self.camera,
            canvas_rect,
            self.selected_edge.as_deref(),
        );
        self.paint_notes(ui, &painter, context, &geometry, canvas_rect, &mut output);
        self.paint_link_hint(ui, context, &geometry, canvas_rect);
        self.paint_drop_hint(ui, &geometry, canvas_rect);

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

        if let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
            && background.clicked()
            && self
                .hit_note(&geometry, context, pointer, canvas_rect)
                .is_none()
        {
            if let Some(edge) =
                hit_edge(context.board, &geometry, &self.camera, canvas_rect, pointer)
            {
                self.selected_edge = Some(edge);
                self.selected = None;
            } else {
                self.selected = None;
                self.selected_edge = None;
                surrender_focus(ui.ctx());
            }
        }

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
        if !canvas_rect.contains(pointer) {
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
        let (secondary_pressed, middle_pressed, secondary_down, middle_down, pointer, delta) = ui
            .input(|input| {
                (
                    input.pointer.button_pressed(PointerButton::Secondary),
                    input.pointer.button_pressed(PointerButton::Middle),
                    input.pointer.secondary_down(),
                    input.pointer.middle_down(),
                    input.pointer.latest_pos(),
                    input.pointer.delta(),
                )
            });

        if self.pan_button.is_none()
            && let Some(pointer) = pointer
            && canvas_rect.contains(pointer)
        {
            if secondary_pressed {
                self.pan_button = Some(PointerButton::Secondary);
            } else if middle_pressed {
                self.pan_button = Some(PointerButton::Middle);
            }
        }

        let Some(button) = self.pan_button else {
            return;
        };
        let still_down = match button {
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
        let pointer = ui.input(|input| input.pointer.interact_pos());
        let primary_down = ui.input(|input| input.pointer.primary_down());
        let primary_released =
            ui.input(|input| input.pointer.button_released(PointerButton::Primary));
        let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));

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
                    self.drop_target = geometry.drop_target(
                        context.board,
                        &drag.id,
                        world,
                        &geometry.world_rects(),
                    );
                }
            } else if primary_released {
                if let Some(pointer) = pointer {
                    let world = self.camera.screen_to_world(pointer, canvas_rect);
                    if drag.moved {
                        output.commands.extend(self.commit_note_drop(
                            context.board,
                            geometry,
                            world,
                        ));
                    }
                }
                self.drag = None;
                self.drop_target = None;
            }
            ui.ctx().request_repaint();
            return;
        }

        if escape {
            if self.editing.is_some() {
                self.finish_edit(context.board, false, output);
            }
            self.selected_edge = None;
            return;
        }

        if self.editing.is_some() && self.focus_requested.is_none() && !self.has_editor_focus(ui) {
            self.finish_edit(context.board, true, output);
        }

        if let Some(pointer) = pointer
            && ui.input(|input| input.pointer.button_pressed(PointerButton::Primary))
            && canvas_rect.contains(pointer)
        {
            if let Some(id) = self.hit_header(geometry, context, pointer, canvas_rect) {
                let world = self.camera.screen_to_world(pointer, canvas_rect);
                let note_rect = geometry
                    .world_rect(&id)
                    .unwrap_or_else(|| Rect::from_min_size(world, Vec2::ZERO));
                self.drag = Some(NoteDrag {
                    id: id.clone(),
                    grab: world - note_rect.min,
                    moved: false,
                });
                self.selected = Some(id);
                self.selected_edge = None;
                surrender_focus(ui.ctx());
            } else if let Some(id) = self.hit_body(geometry, context, pointer, canvas_rect) {
                if self.editing.as_deref() != Some(id.as_str()) {
                    self.finish_edit(context.board, true, output);
                    self.edit_note(context.board, id.clone());
                }
                self.selected = Some(id);
                self.selected_edge = None;
            }
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

    fn handle_edge_input(
        &mut self,
        ui: &mut Ui,
        context: &BoardContext<'_>,
        output: &mut BoardOutput,
    ) {
        if !ui.input(|input| input.key_pressed(egui::Key::Delete)) {
            return;
        }
        if let Some(command) = self.delete_selected_edge(context.board) {
            output.commands.push(command);
            ui.ctx().request_repaint();
        }
    }

    /// Delete the selected arrow (task seam used by the key handler and tests).
    fn delete_selected_edge(&mut self, board: &Board) -> Option<Command> {
        let edge_id = self.selected_edge.clone()?;
        board.edge(&edge_id)?;
        self.selected_edge = None;
        Some(Command::Unlink { edge_id })
    }

    fn hit_note(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        for note in context.board.notes.iter().rev() {
            if note.status == Status::Done {
                continue;
            }
            if let Some(rect) = geometry.screen_rect(&note.id, &self.camera, canvas_rect)
                && rect.contains(pointer)
            {
                return Some(note.id.clone());
            }
        }
        None
    }

    fn hit_header(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        for note in context.board.notes.iter().rev() {
            if note.status == Status::Done {
                continue;
            }
            let Some(rect) = geometry.screen_rect(&note.id, &self.camera, canvas_rect) else {
                continue;
            };
            if !rect.contains(pointer) {
                continue;
            }
            let header_height =
                (CONTAINER_HEADER * self.camera.zoom).clamp(10.0, (rect.height() * 0.5).max(10.0));
            let header = Rect::from_min_size(rect.min, vec2(rect.width(), header_height));
            if header.contains(pointer) {
                return Some(note.id.clone());
            }
            return None;
        }
        None
    }

    fn hit_body(
        &self,
        geometry: &Geometry,
        context: &BoardContext<'_>,
        pointer: Pos2,
        canvas_rect: Rect,
    ) -> Option<NoteId> {
        for note in context.board.notes.iter().rev() {
            if note.status == Status::Done {
                continue;
            }
            if let Some(rect) = geometry.screen_rect(&note.id, &self.camera, canvas_rect)
                && rect.contains(pointer)
                && !geometry.is_container(&note.id)
            {
                return Some(note.id.clone());
            }
        }
        None
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
        let Some(note) = board.note(&drag.id) else {
            return Vec::new();
        };
        if note.parent.is_none() {
            return vec![Command::Pin {
                id: drag.id,
                x: drop_world.x - drag.grab.x,
                y: drop_world.y - drag.grab.y,
            }];
        }
        let target = geometry.drop_target(board, &drag.id, drop_world, &geometry.world_rects());
        vec![Command::SetParent {
            id: drag.id,
            parent: target,
        }]
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
        // Containers paint first so their children are drawn on top.
        let mut order: Vec<&Note> = context.board.notes.iter().collect();
        order.sort_by_key(|note| !geometry.is_container(&note.id));

        for note in order {
            if note.status == Status::Done {
                continue;
            }
            let Some(screen) = geometry.screen_rect(&note.id, &self.camera, canvas_rect) else {
                continue;
            };
            if !canvas_rect.intersects(screen) {
                continue;
            }
            let opacity = opacity_for(note, context.now_utc_ms);
            let palette = theme::note_palette(note.color.clamp(0, 255) as u8);
            let selected = self.selected.as_deref() == Some(note.id.as_str());
            let container = geometry.is_container(&note.id);
            let header_height = (CONTAINER_HEADER * self.camera.zoom)
                .clamp(10.0, (screen.height() * 0.5).max(10.0));
            let header = Rect::from_min_size(screen.min, vec2(screen.width(), header_height));

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
            let text_color = dim(palette.ink, opacity);
            let padding = (theme::NOTE_PADDING * self.camera.zoom).max(2.0);
            painter.text(
                pos2(screen.left() + padding, header.center().y),
                Align2::LEFT_CENTER,
                if title.is_empty() {
                    if container { "Group" } else { "Untitled" }
                } else {
                    title
                },
                FontId::proportional(title_font),
                text_color,
            );

            if !container {
                paint_due_chip(painter, screen, note, self.camera.zoom, opacity);
                paint_priority_dot(painter, header, note.priority, self.camera.zoom, opacity);
                if is_snoozed(note, context.now_utc_ms) {
                    paint_snooze_mark(painter, screen, self.camera.zoom, opacity);
                }
            }

            if self.editing.as_deref() == Some(note.id.as_str()) && !container {
                let editor_rect = Rect::from_min_max(
                    pos2(screen.left() + padding, header.bottom() + padding),
                    pos2(
                        screen.right() - padding,
                        (screen.bottom() - padding).max(header.bottom() + padding + 4.0),
                    ),
                );
                if editor_rect.width() > 8.0 && editor_rect.height() > 8.0 {
                    let editor_id = ui.scope_id().with(("board_editor", note.id.clone()));
                    let font_size = (theme::NOTE_TEXT_SIZE * self.camera.zoom).max(4.0);
                    let response = ui.put(
                        editor_rect,
                        egui::TextEdit::multiline(&mut self.edit_buffer)
                            .id(editor_id)
                            .frame(egui::Frame::NONE)
                            .background_color(Color32::TRANSPARENT)
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
            } else if selected {
                let center = pos2(screen.right() - 16.0, screen.center().y);
                painter.circle_filled(center, LINK_HANDLE_RADIUS, dim(theme::ACCENT, opacity));
                let handle_id = ui.scope_id().with(("link_handle", note.id.clone()));
                let response = ui.interact(
                    Rect::from_center_size(center, Vec2::splat(LINK_HANDLE_RADIUS * 2.5)),
                    handle_id,
                    Sense::drag(),
                );
                if response.drag_started() {
                    self.link = Some(LinkDrag {
                        from: note.id.clone(),
                    });
                }
            }

            let header_id = ui.scope_id().with(("note_header", note.id.clone()));
            let response = ui.interact(header, header_id, Sense::click_and_drag());
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
            if response.clicked() {
                self.selected = Some(note.id.clone());
                self.selected_edge = None;
            }
            if response.double_clicked() {
                self.edit_note(context.board, note.id.clone());
            }
            if response.drag_started()
                && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
            {
                let world = self.camera.screen_to_world(pointer, canvas_rect);
                let note_rect = geometry
                    .world_rect(&note.id)
                    .unwrap_or_else(|| Rect::from_min_size(world, Vec2::ZERO));
                self.drag = Some(NoteDrag {
                    id: note.id.clone(),
                    grab: world - note_rect.min,
                    moved: false,
                });
                self.selected = Some(note.id.clone());
                self.selected_edge = None;
            }
            if response.drag_stopped()
                && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
            {
                let world = self.camera.screen_to_world(pointer, canvas_rect);
                if self.drag.as_ref().is_some_and(|drag| drag.moved) {
                    output
                        .commands
                        .extend(self.commit_note_drop(context.board, geometry, world));
                }
                self.drag = None;
                self.drop_target = None;
            }

            response.context_menu(|ui| {
                if ui.button("Mark done").clicked() {
                    output.commands.push(Command::Complete {
                        id: note.id.clone(),
                    });
                    ui.close();
                }
                if ui.button("Snooze 1 hour").clicked() {
                    if let Some(until) = snooze_after(context.now_utc_ms, 60, context.tz) {
                        output.commands.push(Command::Snooze {
                            id: note.id.clone(),
                            until,
                        });
                    }
                    ui.close();
                }
                if ui.button("Snooze until tomorrow 09:00").clicked() {
                    if let Some(until) = snooze_tomorrow(context.now_utc_ms, context.tz) {
                        output.commands.push(Command::Snooze {
                            id: note.id.clone(),
                            until,
                        });
                    }
                    ui.close();
                }
                if note.snooze.is_some() && ui.button("Unsnooze").clicked() {
                    output.commands.push(Command::Unsnooze {
                        id: note.id.clone(),
                    });
                    ui.close();
                }
            });
        }
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
        if let Some(target) = &self.drop_target
            && let Some(rect) = geometry.screen_rect(target, &self.camera, canvas_rect)
        {
            painter.rect_stroke(
                rect.expand(3.0),
                theme::CARD_CORNER,
                Stroke::new(2.0, theme::ACCENT),
                StrokeKind::Outside,
            );
        }
        if let Some(source) = geometry.screen_rect(&drag.id, &self.camera, canvas_rect) {
            painter.rect_stroke(
                Rect::from_min_size(pointer, source.size()),
                theme::CARD_CORNER,
                Stroke::new(1.0, theme::ACCENT),
                StrokeKind::Outside,
            );
        }
    }

    fn paint_link_hint(
        &self,
        ui: &mut Ui,
        _context: &BoardContext<'_>,
        _geometry: &Geometry,
        _canvas_rect: Rect,
    ) {
        // The line follows the pointer while a link drag is active.
        if self.link.is_none() {
            return;
        }
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        ui.ctx().request_repaint();
    }
}

/// Build the numeric-id mapping and layout for one frame.
struct Geometry {
    layout: Layout,
    note_by_num: HashMap<u64, NoteId>,
    num_by_note: HashMap<NoteId, u64>,
}

impl Geometry {
    fn build(board: &Board, now_utc_ms: i64) -> Self {
        let group_nums: HashMap<GroupId, u64> = board
            .groups
            .iter()
            .enumerate()
            .map(|(index, group)| (group.id.clone(), index as u64))
            .collect();
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
                group: note
                    .group
                    .as_ref()
                    .and_then(|group| group_nums.get(group).copied()),
                done: note.status == Status::Done,
                snoozed: is_snoozed(note, now_utc_ms),
                pinned: note.layout == crate::model::LayoutMode::Pinned,
                x: note.pos.map(|pos| pos.x).unwrap_or(0.0),
                y: note.pos.map(|pos| pos.y).unwrap_or(0.0),
                sort: note.sort,
            })
            .collect();
        let groups: Vec<(u64, i64)> = board
            .groups
            .iter()
            .map(|group| (group_nums[&group.id], group.sort_order))
            .collect();
        let layout = layout::compute(&notes, &groups);

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

    /// The smallest note under the pointer that can accept this drop.
    fn drop_target(
        &self,
        board: &Board,
        dragged: &str,
        world: Pos2,
        rects: &HashMap<NoteId, Rect>,
    ) -> Option<NoteId> {
        let mut candidates: Vec<(&NoteId, &Rect)> = rects
            .iter()
            .filter(|(id, rect)| {
                *id != dragged && rect.contains(world) && !is_ancestor_or_self(board, dragged, id)
            })
            .collect();
        candidates.sort_by(|left, right| {
            let left_area = left.1.width() * left.1.height();
            let right_area = right.1.width() * right.1.height();
            left_area
                .partial_cmp(&right_area)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.first().map(|(id, _)| (*id).clone())
    }
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
    selected: Option<&str>,
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
        let is_selected = selected == Some(edge.id.as_str());
        let color = if is_selected {
            theme::ACCENT
        } else {
            theme::TEXT_MUTED
        };
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [points[0], points[1], points[2], points[3]],
            false,
            Color32::TRANSPARENT,
            Stroke::new(if is_selected { 2.5 } else { 1.5 }, color),
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

fn paint_priority_dot(
    painter: &egui::Painter,
    header: Rect,
    priority: Option<Priority>,
    zoom: f32,
    opacity: f32,
) {
    let Some(priority) = priority else {
        return;
    };
    let color = match priority {
        Priority::High => Color32::from_rgb(0xef, 0x6b, 0x6b),
        Priority::Medium => Color32::from_rgb(0xe0, 0xbf, 0x7a),
        Priority::Low => Color32::from_rgb(0x86, 0xc9, 0x9a),
    };
    painter.circle_filled(
        pos2(header.right() - 10.0 * zoom, header.center().y),
        (3.5 * zoom).clamp(1.5, 5.0),
        dim(color, opacity),
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
                    group: None,
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
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
                    group: None,
                    parent: parent.map(str::to_owned),
                    due: None,
                    remind: None,
                    priority: None,
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
    fn geometry_omits_done_notes_and_marks_containers() {
        let mut store = store("board-geometry");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 1_800_000_000_000);
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
        let geometry = Geometry::build(&board, 1_800_000_000_000);
        assert!(
            geometry.world_rect(&child).is_none(),
            "a done note has no rect"
        );
    }

    #[test]
    fn root_drop_pins_at_the_pointer() {
        let mut store = store("board-pin");
        let (board, root) = board_and_root(&mut store);
        let geometry = Geometry::build(&board, 0);
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: root.clone(),
            grab: vec2(10.0, 5.0),
            moved: true,
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
        let geometry = Geometry::build(&board, 0);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&root].center());
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent: Some(target) }] if id == &child && target == &root
        ));
    }

    #[test]
    fn child_drop_on_itself_unparents() {
        let mut store = store("board-self-drop");
        let (_, root) = board_and_root(&mut store);
        let child = add_note(&mut store, "child", Some(&root));
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&child].center());
        // The parent container is also under the pointer, so the result may be
        // the current parent, but never the note itself.
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent }]
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
        let geometry = Geometry::build(&board, 0);
        let rects = geometry.world_rects();
        let mut view = BoardView::new();
        view.drag = Some(NoteDrag {
            id: child.clone(),
            grab: Vec2::ZERO,
            moved: true,
        });
        let commands = view.commit_note_drop(&board, &geometry, rects[&grandchild].center());
        assert!(matches!(
            commands.as_slice(),
            [Command::SetParent { id, parent }]
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
        let geometry = Geometry::build(&board, 0);
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
        view.selected_edge = Some(edge.clone());
        assert!(matches!(
            view.delete_selected_edge(&board),
            Some(Command::Unlink { edge_id }) if edge_id == edge
        ));
        assert!(view.delete_selected_edge(&board).is_none());
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
        let geometry = Geometry::build(&board, 0);
        let rect = geometry.world_rect(&root).expect("rect");
        assert!((rect.min.x - 123.0).abs() < 0.01);
        assert!((rect.min.y - 456.0).abs() < 0.01);
    }

    #[test]
    fn note_under_pointer_is_found_and_empty_space_is_not() {
        let mut store = store("board-hit");
        let (_, root) = board_and_root(&mut store);
        let board = Board::load(store.connection()).expect("load");
        let geometry = Geometry::build(&board, 0);
        let view = BoardView::new();
        let canvas = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 800.0));
        let context = BoardContext {
            board: &board,
            now_utc_ms: 0,
            tz: TZ,
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
        let geometry = Geometry::build(&board, 0);
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
