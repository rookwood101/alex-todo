use eframe::egui::{self, Color32, Stroke, Visuals};

pub const CANVAS_BG: Color32 = Color32::from_rgb(0x12, 0x14, 0x18);
pub const TOOLBAR_BG: Color32 = Color32::from_rgb(0x19, 0x1c, 0x22);
pub const HINT_BG: Color32 = Color32::from_rgb(0x15, 0x18, 0x1d);
pub const BUTTON_BG: Color32 = Color32::from_rgb(0x25, 0x29, 0x31);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x31, 0x36, 0x41);
pub const BUTTON_ACTIVE: Color32 = Color32::from_rgb(0x3a, 0x40, 0x4d);
pub const BORDER: Color32 = Color32::from_rgb(0x2b, 0x30, 0x39);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xe9, 0xec, 0xf2);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8e, 0x96, 0xa6);
pub const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x9c, 0xff);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x3d, 0x55, 0x92);
pub const SAVE_OK: Color32 = Color32::from_rgb(0x86, 0xc9, 0x9a);
pub const SAVE_WARN: Color32 = Color32::from_rgb(0xe0, 0xbf, 0x7a);
pub const SAVE_ERROR: Color32 = Color32::from_rgb(0xef, 0x8b, 0x8b);
pub const GRID_MINOR: Color32 = Color32::from_rgb(0x1c, 0x20, 0x27);
pub const GRID_MAJOR: Color32 = Color32::from_rgb(0x27, 0x2c, 0x36);
pub const GRID_AXIS: Color32 = Color32::from_rgb(0x35, 0x3d, 0x4a);
pub const NOTE_INK: Color32 = Color32::from_rgb(0x25, 0x22, 0x1d);
pub const NOTE_HINT: Color32 = Color32::WHITE;
pub const NOTE_TEXT_CURSOR: Color32 = Color32::from_rgb(0xb5, 0x1c, 0x5e);
pub const NOTE_CURSOR_WIDTH: f32 = 3.0;
pub const NOTE_SHADOW: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 90);
pub const NOTE_EDGE: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 28);

pub const NOTE_WIDTH: f32 = 260.0;
pub const NOTE_HEIGHT: f32 = 200.0;
pub const NOTE_PADDING: f32 = 14.0;
pub const NOTE_HEADER_HEIGHT: f32 = 30.0;
pub const NOTE_TEXT_SIZE: f32 = 17.0;
pub const CARD_CORNER: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotePalette {
    pub fill: Color32,
    pub header: Color32,
    pub ink: Color32,
}

pub const NOTE_PALETTE: [NotePalette; 5] = [
    NotePalette {
        fill: Color32::from_rgb(0xf0, 0xd4, 0x86),
        header: Color32::from_rgb(0xe6, 0xc5, 0x70),
        ink: NOTE_INK,
    },
    NotePalette {
        fill: Color32::from_rgb(0xe8, 0xa7, 0xb2),
        header: Color32::from_rgb(0xdd, 0x94, 0xa1),
        ink: NOTE_INK,
    },
    NotePalette {
        fill: Color32::from_rgb(0x93, 0xc6, 0xe8),
        header: Color32::from_rgb(0x81, 0xb6, 0xda),
        ink: NOTE_INK,
    },
    NotePalette {
        fill: Color32::from_rgb(0xa8, 0xd8, 0xad),
        header: Color32::from_rgb(0x94, 0xc7, 0x9b),
        ink: NOTE_INK,
    },
    NotePalette {
        fill: Color32::from_rgb(0xba, 0xac, 0xe6),
        header: Color32::from_rgb(0xa7, 0x99, 0xd6),
        ink: NOTE_INK,
    },
];

pub fn note_palette(color: u8) -> NotePalette {
    NOTE_PALETTE[color as usize % NOTE_PALETTE.len()]
}

pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.panel_fill = TOOLBAR_BG;
    visuals.window_fill = TOOLBAR_BG;
    visuals.extreme_bg_color = CANVAS_BG;
    visuals.faint_bg_color = BUTTON_BG;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.noninteractive.bg_fill = TOOLBAR_BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_MUTED);
    visuals.widgets.inactive.bg_fill = BUTTON_BG;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.hovered.bg_fill = BUTTON_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.active.bg_fill = BUTTON_ACTIVE;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    ctx.set_visuals(visuals);
}

/// The red "!" that marks an important note.
pub const IMPORTANT: Color32 = Color32::from_rgb(0xef, 0x4b, 0x4b);
