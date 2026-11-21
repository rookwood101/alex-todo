use eframe::egui::{
    self, Align, Align2, CursorIcon, FontId, Layout, PointerButton, Rect, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2,
};

use crate::app::{AppAction, RustboardApp};
use crate::board::{Note, NoteId};
use crate::camera::Camera;
use crate::theme;

pub(crate) fn show(app: &mut RustboardApp, ui: &mut Ui) {
    let available = ui.available_size();
    if !available.x.is_finite()
        || !available.y.is_finite()
        || available.x < 1.0
        || available.y < 1.0
    {
        return;
    }

    let (canvas_rect, background_response) = ui.allocate_exact_size(available, Sense::click());
    if canvas_rect.width() < 1.0 || canvas_rect.height() < 1.0 {
        return;
    }
    app.canvas_rect = canvas_rect;

    if app.fit_pending {
        fit_view(app, canvas_rect);
        app.fit_pending = false;
    }

    handle_wheel(app, ui, canvas_rect);
    handle_pan(app, ui, canvas_rect);
    handle_note_drag(app, ui, canvas_rect);

    let painter = ui.painter_at(canvas_rect);
    painter.rect_filled(canvas_rect, 0, theme::CANVAS_BG);
    paint_grid(&painter, &app.camera, canvas_rect);
    paint_notes(app, ui, canvas_rect);

    if app.board.notes.is_empty() {
        painter.text(
            canvas_rect.center() + vec2(0.0, -10.0),
            Align2::CENTER_CENTER,
            "Your canvas is empty",
            FontId::proportional(22.0),
            theme::TEXT_PRIMARY,
        );
        painter.text(
            canvas_rect.center() + vec2(0.0, 20.0),
            Align2::CENTER_CENTER,
            "Use + New note or Ctrl+N to start",
            FontId::proportional(13.0),
            theme::TEXT_MUTED,
        );
    }

    if app.pan_button.is_some() || app.note_drag.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if background_response.clicked() {
        let clicked_note = ui
            .input(|input| input.pointer.interact_pos())
            .is_some_and(|pos| {
                app.board
                    .notes
                    .iter()
                    .any(|note| note_screen_rect(note, &app.camera, canvas_rect).contains(pos))
            });
        if !clicked_note {
            app.update(AppAction::Select(None));
            surrender_focus(ui.ctx());
        }
    }
}

fn fit_view(app: &mut RustboardApp, canvas_rect: Rect) {
    if let Some(bounds) = app.board.bounds() {
        app.camera.fit_to_bounds(bounds, canvas_rect);
    } else {
        app.camera.reset_to_origin();
    }
}

fn handle_wheel(app: &mut RustboardApp, ui: &mut Ui, canvas_rect: Rect) {
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

    // Mouse-wheel-up (positive scroll) zooms in. The exponential curve keeps
    // each notch proportional, while the clamp prevents huge trackpad jumps.
    let factor = (scroll.y * 0.0025).exp().clamp(0.5, 2.0);
    app.update(AppAction::ZoomAt {
        screen_pos: pointer,
        factor,
    });
    ui.ctx().request_repaint();
}

fn handle_pan(app: &mut RustboardApp, ui: &mut Ui, canvas_rect: Rect) {
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

    let mut started = false;
    if app.pan_button.is_none()
        && let Some(pointer) = pointer
        && canvas_rect.contains(pointer)
    {
        if secondary_pressed {
            app.pan_button = Some(PointerButton::Secondary);
            started = true;
        } else if middle_pressed {
            app.pan_button = Some(PointerButton::Middle);
            started = true;
        }
    }

    let Some(button) = app.pan_button else {
        return;
    };
    let still_down = match button {
        PointerButton::Secondary => secondary_down,
        PointerButton::Middle => middle_down,
        _ => false,
    };
    if !still_down {
        app.pan_button = None;
        return;
    }

    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    ui.ctx().request_repaint();
    if !started {
        app.update(AppAction::PanBy(delta));
    }
}

fn handle_note_drag(app: &mut RustboardApp, ui: &mut Ui, canvas_rect: Rect) {
    let (primary_pressed, primary_down, pointer, delta) = ui.input(|input| {
        (
            input.pointer.button_pressed(PointerButton::Primary),
            input.pointer.primary_down(),
            input.pointer.latest_pos(),
            input.pointer.delta(),
        )
    });

    if app.note_drag.is_none() {
        let Some(pointer) = pointer else {
            return;
        };
        if !primary_pressed || !canvas_rect.contains(pointer) {
            return;
        }

        // Find the frontmost card under the cursor. A card body deliberately
        // blocks this path so text selection stays with the inline editor.
        let mut hit = None;
        for note in app.board.notes.iter().rev() {
            let card_rect = note_screen_rect(note, &app.camera, canvas_rect);
            if card_rect.contains(pointer) {
                let header_rect = note_header_rect(note, &app.camera, canvas_rect);
                if header_rect.contains(pointer) {
                    hit = Some(note.id);
                }
                break;
            }
        }

        if let Some(id) = hit {
            app.update(AppAction::Select(Some(id)));
            app.note_drag = Some(id);
            surrender_focus(ui.ctx());
            ui.ctx().request_repaint();
        }
        return;
    }

    let Some(id) = app.note_drag else {
        return;
    };
    if !primary_down || !app.board.notes.iter().any(|note| note.id == id) {
        app.note_drag = None;
        return;
    }

    if delta != Vec2::ZERO {
        app.update(AppAction::MoveNote {
            id,
            screen_delta: delta,
        });
        ui.ctx().request_repaint();
    }
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
            let stroke = if index.rem_euclid(5) == 0 {
                major
            } else {
                minor
            };
            painter.vline(screen_x, canvas_rect.y_range(), stroke);
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
            let stroke = if index.rem_euclid(5) == 0 {
                major
            } else {
                minor
            };
            painter.hline(canvas_rect.x_range(), screen_y, stroke);
        }
    }

    // A weak origin cross makes it easy to stay oriented on an infinite plane.
    let origin = camera.world_to_screen(pos2(0.0, 0.0), canvas_rect);
    let axis = Stroke::new(1.0, theme::GRID_AXIS);
    if canvas_rect.left() <= origin.x && origin.x <= canvas_rect.right() {
        painter.vline(origin.x, canvas_rect.y_range(), axis);
    }
    if canvas_rect.top() <= origin.y && origin.y <= canvas_rect.bottom() {
        painter.hline(canvas_rect.x_range(), origin.y, axis);
    }
}

fn paint_notes(app: &mut RustboardApp, ui: &mut Ui, canvas_rect: Rect) {
    let zoom = app.camera.zoom;
    let mut selection_update: Option<Option<NoteId>> = None;
    let focus_target = app.pending_focus.take();
    let mut focus_used = false;

    for index in 0..app.board.notes.len() {
        let note = &mut app.board.notes[index];
        let card_rect = note_screen_rect(note, &app.camera, canvas_rect);
        if !canvas_rect.intersects(card_rect) {
            continue;
        }

        let palette = note.palette();
        let selected = app.selected == Some(note.id);
        let corner = ((theme::CARD_CORNER as f32) * zoom).clamp(3.0, 16.0) as u8;
        let header_rect = note_header_rect(note, &app.camera, canvas_rect);

        paint_card(
            &ui.painter_at(canvas_rect),
            card_rect,
            header_rect,
            corner,
            palette,
            selected,
        );
        paint_grip(&ui.painter_at(canvas_rect), header_rect, palette, zoom);

        let header_id = ui.scope_id().with(("note_header", note.id));
        let header_response = ui.interact(header_rect, header_id, Sense::click());
        if header_response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if app.note_drag == Some(note.id) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }

        let padding = theme::NOTE_PADDING * zoom;
        let editor_rect = Rect::from_min_max(
            pos2(
                card_rect.left() + padding,
                header_rect.bottom() + padding * 0.25,
            ),
            pos2(card_rect.right() - padding, card_rect.bottom() - padding),
        );
        if editor_rect.width() <= 8.0 || editor_rect.height() <= 8.0 {
            continue;
        }

        let editor_id = ui.scope_id().with(("note_editor", note.id));
        let mut editor_ui = ui.new_child(
            UiBuilder::new()
                .id_salt(("note_editor_ui", note.id))
                .max_rect(editor_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        editor_ui.set_clip_rect(editor_rect.intersect(canvas_rect));
        editor_ui.visuals_mut().text_cursor.stroke =
            Stroke::new(theme::NOTE_CURSOR_WIDTH, theme::NOTE_TEXT_CURSOR);
        editor_ui.visuals_mut().weak_text_color = Some(theme::NOTE_HINT);

        let font_size = (theme::NOTE_TEXT_SIZE * zoom).max(4.0);
        let hint = egui::RichText::new("Type something").font(FontId::proportional(font_size));
        let text_edit = egui::TextEdit::multiline(&mut note.text)
            .id(editor_id)
            .frame(egui::Frame::NONE)
            .background_color(egui::Color32::TRANSPARENT)
            .margin(egui::Margin::same(0))
            .text_color(theme::NOTE_INK)
            .font(FontId::proportional(font_size))
            .desired_width(editor_rect.width())
            .hint_text(hint);
        let response = editor_ui
            .add_sized(editor_rect.size(), text_edit)
            .accessible_name(format!("Note text {}", note.id));

        if response.clicked() || response.has_focus() {
            selection_update = Some(Some(note.id));
        }
        if focus_target == Some(note.id) {
            response.request_focus();
            focus_used = true;
        }
    }

    if let Some(selection) = selection_update {
        app.update(AppAction::Select(selection));
    }
    app.pending_focus = if focus_used { None } else { focus_target };
}

fn note_screen_rect(note: &Note, camera: &Camera, canvas_rect: Rect) -> Rect {
    let world = note.rect();
    Rect::from_min_max(
        camera.world_to_screen(world.min, canvas_rect),
        camera.world_to_screen(world.max, canvas_rect),
    )
}

fn note_header_rect(note: &Note, camera: &Camera, canvas_rect: Rect) -> Rect {
    let card_rect = note_screen_rect(note, camera, canvas_rect);
    let header_height =
        (theme::NOTE_HEADER_HEIGHT * camera.zoom).clamp(6.0, (card_rect.height() * 0.35).max(6.0));
    Rect::from_min_size(card_rect.min, vec2(card_rect.width(), header_height))
}

fn paint_card(
    painter: &egui::Painter,
    card_rect: Rect,
    header_rect: Rect,
    corner: u8,
    palette: theme::NotePalette,
    selected: bool,
) {
    let shadow_offset = 4.0;
    let shadow_rect =
        Rect::from_min_size(card_rect.min + vec2(0.0, shadow_offset), card_rect.size());
    painter.rect_filled(shadow_rect, corner, theme::NOTE_SHADOW);
    painter.rect_filled(card_rect, corner, palette.fill);
    painter.rect_filled(header_rect, corner, palette.header);
    painter.hline(
        card_rect.x_range(),
        header_rect.bottom(),
        Stroke::new(1.0, theme::NOTE_EDGE),
    );

    let stroke = if selected {
        Stroke::new(2.0, theme::ACCENT)
    } else {
        Stroke::new(1.0, theme::NOTE_EDGE)
    };
    painter.rect_stroke(card_rect, corner, stroke, StrokeKind::Inside);
}

fn paint_grip(painter: &egui::Painter, header_rect: Rect, palette: theme::NotePalette, zoom: f32) {
    let radius = (1.3 * zoom).clamp(0.65, 2.0);
    let gap = 4.6 * zoom;
    let x = header_rect.left() + 11.0 * zoom;
    let center_y = header_rect.center().y;
    let color = palette.ink.gamma_multiply(0.38);

    for row in -1..=1 {
        for column in 0..2 {
            painter.circle_filled(
                pos2(x + column as f32 * gap, center_y + row as f32 * gap),
                radius,
                color,
            );
        }
    }
}

fn surrender_focus(ctx: &egui::Context) {
    if let Some(focused) = ctx.memory(|memory| memory.focused()) {
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
    }
}
