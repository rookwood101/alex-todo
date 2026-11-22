use eframe::egui::{
    Context, Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2,
    RawInput, Rect, TouchPhase, Vec2, pos2, vec2,
};

use crate::app::{PersistedState, RustboardApp};
use crate::board::NoteId;
use crate::persist::StateStore;
use crate::theme;

const FRAME_SECONDS: f64 = 1.0 / 60.0;

struct Headless {
    ctx: Context,
    app: RustboardApp,
    screen: Rect,
    time: f64,
}

impl Headless {
    fn new() -> Self {
        Self::with_app(RustboardApp::for_test())
    }

    fn with_app(app: RustboardApp) -> Self {
        let ctx = Context::default();
        theme::apply(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0));
        let mut harness = Self {
            ctx,
            app,
            screen,
            time: 0.0,
        };
        harness.run_frame(Vec::new());
        harness.run_frame(Vec::new());
        harness
    }

    fn with_store(path: impl Into<std::path::PathBuf>) -> Self {
        Self::with_app(RustboardApp::for_test_with_store(path))
    }

    fn run_frame(&mut self, events: Vec<Event>) {
        self.time += FRAME_SECONDS;
        let raw = RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.time),
            focused: true,
            events,
            ..Default::default()
        };
        let Self { ctx, app, .. } = self;
        let mut output = ctx.run_ui(raw, |ui| app.show_ui(ui));
        output.textures_delta.clear();
    }

    fn canvas_rect(&self) -> Rect {
        self.app.canvas_rect
    }

    fn note_screen_rect(&self, id: NoteId) -> Rect {
        let note = self
            .app
            .board
            .notes
            .iter()
            .find(|note| note.id == id)
            .expect("test note exists");
        let world = note.rect();
        Rect::from_min_max(
            self.app
                .camera
                .world_to_screen(world.min, self.app.canvas_rect),
            self.app
                .camera
                .world_to_screen(world.max, self.app.canvas_rect),
        )
    }

    fn note_header_center(&self, id: NoteId) -> Pos2 {
        let card = self.note_screen_rect(id);
        let header_height = (theme::NOTE_HEADER_HEIGHT * self.app.camera.zoom)
            .clamp(6.0, (card.height() * 0.35).max(6.0));
        pos2(card.center().x, card.top() + header_height * 0.5)
    }

    fn note_text_center(&self, id: NoteId) -> Pos2 {
        let card = self.note_screen_rect(id);
        let header_height = (theme::NOTE_HEADER_HEIGHT * self.app.camera.zoom)
            .clamp(6.0, (card.height() * 0.35).max(6.0));
        pos2(card.center().x, card.top() + header_height + 45.0)
    }

    fn pointer_move(&mut self, pos: Pos2) {
        self.run_frame(vec![Event::PointerMoved(pos)]);
    }

    fn pointer_button(&mut self, pos: Pos2, button: PointerButton, pressed: bool) {
        self.run_frame(vec![Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        }]);
    }

    fn wheel(&mut self, delta: Vec2) {
        self.run_frame(vec![Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta,
            phase: TouchPhase::Move,
            source: MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        }]);
    }

    fn drag(&mut self, button: PointerButton, from: Pos2, to: Pos2) {
        self.pointer_move(from);
        self.pointer_button(from, button, true);
        self.pointer_move(to);
        self.pointer_button(to, button, false);
        self.run_frame(Vec::new());
    }
}

fn assert_close(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() < 0.05,
        "{what}: expected {expected}, got {actual}"
    );
}

#[test]
fn wheel_zoom_works_over_note_text_and_keeps_anchor() {
    let mut harness = Headless::new();
    let anchor = harness.note_text_center(1);
    harness.pointer_move(anchor);

    let before_zoom = harness.app.camera.zoom;
    let before_world = harness
        .app
        .camera
        .screen_to_world(anchor, harness.canvas_rect());

    harness.wheel(vec2(0.0, 80.0));
    harness.run_frame(Vec::new());
    harness.run_frame(Vec::new());

    let after_zoom = harness.app.camera.zoom;
    let after_world = harness
        .app
        .camera
        .screen_to_world(anchor, harness.canvas_rect());

    assert!(
        after_zoom > before_zoom,
        "wheel over note text should zoom in, before={before_zoom}, after={after_zoom}"
    );
    assert_close(after_world.x, before_world.x, "zoom anchor x");
    assert_close(after_world.y, before_world.y, "zoom anchor y");
}

#[test]
fn right_and_middle_drag_pan_anywhere_over_notes() {
    let mut harness = Headless::new();
    let start = harness.note_header_center(2);
    let delta = vec2(120.0, -70.0);

    let zoom = harness.app.camera.zoom;
    let before = harness.app.camera.center;
    harness.drag(PointerButton::Secondary, start, start + delta);
    assert_close(
        harness.app.camera.center[0],
        before[0] - delta.x / zoom,
        "secondary pan x",
    );
    assert_close(
        harness.app.camera.center[1],
        before[1] - delta.y / zoom,
        "secondary pan y",
    );
    assert_eq!(harness.app.note_drag, None);

    harness.app.camera.center = before;
    harness.run_frame(Vec::new());
    harness.drag(PointerButton::Middle, start, start + delta);
    assert_close(
        harness.app.camera.center[0],
        before[0] - delta.x / zoom,
        "middle pan x",
    );
    assert_close(
        harness.app.camera.center[1],
        before[1] - delta.y / zoom,
        "middle pan y",
    );
    assert_eq!(harness.app.note_drag, None);
}

#[test]
fn note_header_drag_moves_before_button_release() {
    let mut harness = Headless::new();
    harness.app.board.bring_to_front(1);
    harness.run_frame(Vec::new());

    let start = harness.note_header_center(1);
    let delta = vec2(90.0, 55.0);
    let before = harness
        .app
        .board
        .notes
        .iter()
        .find(|note| note.id == 1)
        .expect("note 1")
        .pos;
    let zoom = harness.app.camera.zoom;

    harness.pointer_move(start);
    harness.pointer_button(start, PointerButton::Primary, true);
    assert_eq!(harness.app.note_drag, Some(1));
    assert_eq!(harness.app.selected, Some(1));

    harness.pointer_move(start + delta);
    let after = harness
        .app
        .board
        .notes
        .iter()
        .find(|note| note.id == 1)
        .expect("note 1")
        .pos;
    assert_close(after[0], before[0] + delta.x / zoom, "drag note x");
    assert_close(after[1], before[1] + delta.y / zoom, "drag note y");
    assert_eq!(
        harness.app.board.notes.last().map(|note| note.id),
        Some(1),
        "dragged note should be frontmost"
    );

    harness.pointer_button(start + delta, PointerButton::Primary, false);
    assert_eq!(harness.app.note_drag, None);
}

#[test]
fn clicking_note_text_focuses_editor_and_accepts_text() {
    let mut harness = Headless::new();
    harness.app.board.bring_to_front(2);
    harness.run_frame(Vec::new());

    let text_pos = harness.note_text_center(2);
    harness.pointer_move(text_pos);
    harness.pointer_button(text_pos, PointerButton::Primary, true);
    harness.pointer_button(text_pos, PointerButton::Primary, false);
    harness.run_frame(Vec::new());
    harness.run_frame(Vec::new());

    let before = harness
        .app
        .board
        .notes
        .iter()
        .find(|note| note.id == 2)
        .expect("note 2")
        .text
        .clone();
    harness.run_frame(vec![Event::Text("X".to_owned())]);
    harness.run_frame(Vec::new());

    let after = &harness
        .app
        .board
        .notes
        .iter()
        .find(|note| note.id == 2)
        .expect("note 2")
        .text;
    assert_ne!(before, *after, "typing should modify the focused note");
    assert!(after.contains('X'), "typed character should reach the note");
}

#[test]
fn ctrl_n_adds_a_note_when_not_editing() {
    let mut harness = Headless::new();
    let before = harness.app.board.notes.len();

    harness.run_frame(vec![Event::Key {
        key: Key::N,
        physical_key: Some(Key::N),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::CTRL,
    }]);
    harness.run_frame(vec![Event::Key {
        key: Key::N,
        physical_key: Some(Key::N),
        pressed: false,
        repeat: false,
        modifiers: Modifiers::CTRL,
    }]);
    harness.run_frame(Vec::new());

    assert_eq!(harness.app.board.notes.len(), before + 1);
    assert!(harness.app.selected.is_some());
}

#[test]
fn typing_in_a_note_is_written_to_the_state_file() {
    let path = crate::persist::scratch_dir("headless-save").join("board.ron");

    let mut harness = Headless::with_store(&path);
    harness.app.board.bring_to_front(2);
    harness.run_frame(Vec::new());

    let text_pos = harness.note_text_center(2);
    harness.pointer_move(text_pos);
    harness.pointer_button(text_pos, PointerButton::Primary, true);
    harness.pointer_button(text_pos, PointerButton::Primary, false);
    harness.run_frame(Vec::new());
    harness.run_frame(Vec::new());

    harness.run_frame(vec![Event::Text("Q".to_owned())]);
    // Writes are coalesced for a short window, so let the scheduled save run.
    std::thread::sleep(std::time::Duration::from_millis(300));
    harness.run_frame(Vec::new());

    assert!(path.is_file(), "typing should create {}", path.display());

    let saved: PersistedState = StateStore::at(&path)
        .load()
        .expect("saved state loads")
        .expect("saved state exists");
    let note = saved
        .board
        .notes
        .iter()
        .find(|note| note.id == 2)
        .expect("saved note 2");
    assert!(
        note.text.contains('Q'),
        "typed text should be on disk, got {:?}",
        note.text
    );
}
