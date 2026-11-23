//! Headless integration tests for the app shell (task 170).
//!
//! Each test drives the real [RustboardApp] with an egui context and a private
//! SQLite board in target/test-tmp. No window, no network and no live model.

use eframe::egui::{
    Context, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2,
};

use crate::app::RustboardApp;
use crate::command::{Actor, Command};
use crate::config::Config;
use crate::model::{Board, Status};
use crate::store::{StorePaths, schema, scratch_dir};
use crate::theme;

const FRAME_SECONDS: f64 = 1.0 / 60.0;
/// Ctrl as egui-winit reports it on Windows (ctrl and command both set).
const CTRL: Modifiers = Modifiers {
    alt: false,
    ctrl: true,
    shift: false,
    mac_cmd: false,
    command: true,
};

struct Headless {
    ctx: Context,
    app: RustboardApp,
    screen: Rect,
    time: f64,
    modifiers: Modifiers,
}

impl Headless {
    fn new(name: &str) -> Self {
        Self::with_paths(StorePaths::from_home(scratch_dir(name)))
    }

    fn with_paths(paths: StorePaths) -> Self {
        let ctx = Context::default();
        theme::apply(&ctx);
        let app = RustboardApp::for_test(paths, ctx.clone());
        let mut headless = Self {
            ctx,
            app,
            screen: Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0)),
            time: 0.0,
            modifiers: Modifiers::NONE,
        };
        headless.run_frame(Vec::new());
        headless
    }

    fn run_frame(&mut self, mut events: Vec<Event>) {
        events.insert(0, Event::ModifiersChanged(self.modifiers));
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

    fn key(&mut self, key: Key) {
        self.key_with(key, Modifiers::NONE);
    }

    fn key_with(&mut self, key: Key, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.run_frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }]);
        self.modifiers = Modifiers::NONE;
        self.run_frame(Vec::new());
    }

    fn pointer(&mut self, pos: Pos2, pressed: bool, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.run_frame(vec![
            Event::PointerMoved(pos),
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers,
            },
        ]);
    }

    /// Press and release at one point, then settle one frame. The time step
    /// keeps two clicks from counting as a double-click.
    fn click_with(&mut self, pos: Pos2, modifiers: Modifiers) {
        self.time += 1.0;
        self.pointer(pos, true, modifiers);
        self.pointer(pos, false, modifiers);
        self.run_frame(Vec::new());
    }

    fn click(&mut self, pos: Pos2) {
        self.click_with(pos, Modifiers::NONE);
    }

    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.time += 1.0;
        self.pointer(from, true, Modifiers::NONE);
        for step in 1..=4 {
            let t = step as f32 / 4.0;
            self.run_frame(vec![Event::PointerMoved(from + (to - from) * t)]);
        }
        self.pointer(to, false, Modifiers::NONE);
        self.run_frame(Vec::new());
    }

    fn type_text(&mut self, text: &str) {
        self.run_frame(vec![Event::Text(text.to_owned())]);
    }

    fn note_rect(&self, id: &str) -> Rect {
        self.app
            .view
            .screen_rect_for(&self.app.board, id, self.app.canvas_rect)
            .expect("note is on screen")
    }

    /// A canvas point outside `avoid` that no floating panel covers.
    fn empty_point(&self, avoid: Rect) -> Pos2 {
        let canvas = self.app.canvas_rect;
        let candidates = [
            pos2(avoid.min.x - 60.0, avoid.center().y),
            pos2(avoid.center().x, avoid.min.y - 40.0),
            pos2(avoid.max.x + 60.0, avoid.center().y),
            pos2(avoid.center().x, avoid.max.y + 40.0),
        ];
        candidates
            .into_iter()
            .find(|point| {
                canvas.contains(*point)
                    && self
                        .ctx
                        .layer_id_at(*point)
                        .is_none_or(|layer| layer.order == eframe::egui::Order::Background)
            })
            .expect("an empty canvas point")
    }

    fn note_ids(&self) -> Vec<String> {
        self.app
            .board
            .notes
            .iter()
            .map(|note| note.id.clone())
            .collect()
    }
}

fn add_note(app: &mut RustboardApp, text: &str) {
    app.apply_commands(
        vec![Command::AddNote {
            text: text.to_owned(),
            container: None,
            at_root: true,
            parent: None,
            due: None,
            remind: None,
            important: false,
            source: "test".to_owned(),
        }],
        "test",
    )
    .expect("add note");
}

fn external_add(paths: &StorePaths, text: &str) {
    let mut store = schema::open_board(paths).expect("second connection");
    store
        .apply(
            &[Command::AddNote {
                text: text.to_owned(),
                container: None,
                at_root: true,
                parent: None,
                due: None,
                remind: None,
                important: false,
                source: "agent".to_owned(),
            }],
            Actor::Agent,
            Some("external"),
        )
        .expect("external write");
}

#[test]
fn capture_through_the_composer_creates_a_db_row() {
    let mut headless = Headless::new("app-capture");
    headless.app.composer.set_text("buy milk tomorrow");
    headless.app.composer.request_focus();
    headless.run_frame(Vec::new());
    headless.key(Key::Enter);
    headless.run_frame(Vec::new());

    let inbox = headless
        .app
        .board
        .inbox()
        .expect("capture creates the Inbox")
        .clone();
    let notes: Vec<_> = headless
        .app
        .board
        .notes
        .iter()
        .filter(|note| !note.inbox)
        .collect();
    assert_eq!(notes.len(), 1, "composer Enter creates one note");
    let note = notes[0];
    assert_eq!(note.text, "buy milk");
    assert_eq!(
        note.parent.as_ref(),
        Some(&inbox.id),
        "it lands in the Inbox"
    );
    assert!(note.due.is_some(), "capture chips were applied");

    let stored = Board::load(headless.app.store.as_ref().expect("store").connection())
        .expect("stored board");
    assert_eq!(stored.notes.len(), 2, "the note and the Inbox");
}

#[test]
fn an_external_commit_reloads_the_board() {
    let mut headless = Headless::new("app-external");
    add_note(&mut headless.app, "mine");
    let paths = headless.app.paths.clone();
    external_add(&paths, "from another connection");

    headless.app.poll_for_test();
    assert_eq!(headless.app.board.notes.len(), 2);
    assert!(
        headless
            .app
            .board
            .notes
            .iter()
            .any(|note| note.text == "from another connection"),
        "the external note is visible"
    );
}

#[test]
fn a_focused_editor_defers_the_reload_until_it_closes() {
    let mut headless = Headless::new("app-defer");
    add_note(&mut headless.app, "editing");
    let id = headless.app.board.notes[0].id.clone();
    headless.app.view.edit_note(&headless.app.board, id);

    let paths = headless.app.paths.clone();
    external_add(&paths, "external while editing");

    headless.app.poll_for_test();
    assert!(headless.app.reload_deferred, "the reload is deferred");
    assert_eq!(
        headless.app.board.notes.len(),
        1,
        "the board did not reload under the editor"
    );

    headless.app.view.stop_editing();
    headless.app.poll_for_test();
    assert!(!headless.app.reload_deferred);
    assert_eq!(headless.app.board.notes.len(), 2);
}

#[test]
fn undo_and_redo_round_trip_through_the_store() {
    let mut headless = Headless::new("app-undo");
    add_note(&mut headless.app, "reversible");
    assert_eq!(headless.app.board.notes.len(), 1);

    headless.app.undo();
    assert!(headless.app.board.notes.is_empty(), "undo removed the note");

    headless.app.redo();
    assert_eq!(headless.app.board.notes.len(), 1, "redo restored the note");
    assert_eq!(headless.app.board.notes[0].text, "reversible");
}

#[test]
fn a_leftover_board_ron_is_ignored() {
    let paths = StorePaths::from_home(scratch_dir("app-legacy"));
    std::fs::create_dir_all(paths.home()).expect("home");
    let legacy = paths.home().join("board.ron");
    std::fs::write(&legacy, "this is the old RON board").expect("legacy file");

    let headless = Headless::with_paths(paths);
    assert!(headless.app.board.notes.is_empty());
    assert_eq!(
        std::fs::read_to_string(&legacy).expect("legacy file survives"),
        "this is the old RON board",
        "the app never touches the old state file"
    );
}

#[test]
fn a_click_selects_and_a_second_click_edits() {
    let mut headless = Headless::new("app-click-select");
    add_note(&mut headless.app, "first");
    headless.run_frame(Vec::new());
    let id = headless.note_ids()[0].clone();
    let center = headless.note_rect(&id).center();

    headless.click(center);
    assert_eq!(headless.app.view.selected, vec![id.clone()]);
    assert!(!headless.app.view.is_editing(), "one click only selects");

    headless.click(center);
    assert_eq!(headless.app.view.editing_note(), Some(&id));
}

#[test]
fn escape_saves_the_edit_then_clears_the_selection() {
    let mut headless = Headless::new("app-escape-saves");
    add_note(&mut headless.app, "old");
    headless.run_frame(Vec::new());
    let id = headless.note_ids()[0].clone();
    headless.app.view.edit_note(&headless.app.board, id.clone());
    headless.run_frame(Vec::new());
    headless.run_frame(Vec::new());
    headless.type_text(" text");

    headless.key(Key::Escape);
    assert!(!headless.app.view.is_editing());
    assert_eq!(headless.app.board.note(&id).expect("note").text, "old text");
    assert_eq!(
        headless.app.view.selected,
        vec![id],
        "the first Escape only leaves the editor"
    );

    headless.key(Key::Escape);
    assert!(headless.app.view.selected.is_empty());
}

#[test]
fn delete_removes_the_selection_and_undo_restores_it() {
    let mut headless = Headless::new("app-delete");
    add_note(&mut headless.app, "doomed");
    add_note(&mut headless.app, "keep");
    headless.run_frame(Vec::new());
    let doomed = headless
        .app
        .board
        .notes
        .iter()
        .find(|note| note.text == "doomed")
        .expect("note")
        .id
        .clone();
    headless.click(headless.note_rect(&doomed).center());
    headless.key(Key::Delete);
    assert_eq!(headless.app.board.notes.len(), 1);
    assert!(headless.app.board.note(&doomed).is_none());

    headless.key_with(Key::Z, CTRL);
    assert!(
        headless.app.board.note(&doomed).is_some(),
        "undo restores it"
    );
}

#[test]
fn backspace_also_deletes_but_not_while_editing() {
    let mut headless = Headless::new("app-backspace");
    add_note(&mut headless.app, "text");
    headless.run_frame(Vec::new());
    let id = headless.note_ids()[0].clone();
    let center = headless.note_rect(&id).center();

    headless.click(center);
    headless.click(center);
    assert!(headless.app.view.is_editing());
    headless.key(Key::Backspace);
    headless.key(Key::Delete);
    assert_eq!(headless.app.board.notes.len(), 1, "keys edit the text");

    headless.key(Key::Escape);
    headless.key(Key::Backspace);
    assert!(headless.app.board.notes.is_empty());
}

#[test]
fn a_box_drag_and_shift_click_build_a_multi_selection() {
    let mut headless = Headless::new("app-marquee");
    add_note(&mut headless.app, "a");
    add_note(&mut headless.app, "b");
    headless.run_frame(Vec::new());
    let ids = headless.note_ids();
    let first = headless.note_rect(&ids[0]);
    let second = headless.note_rect(&ids[1]);
    let all = first.union(second);

    headless.drag(all.min - vec2(20.0, 20.0), all.max + vec2(20.0, 20.0));
    let mut selected = headless.app.view.selected.clone();
    selected.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(selected, expected, "the box selects both notes");

    headless.click(first.center());
    assert_eq!(
        headless.app.view.selected.len(),
        2,
        "clicking a member keeps the group"
    );
    headless.click_with(first.center(), Modifiers::SHIFT);
    assert_eq!(headless.app.view.selected, vec![ids[1].clone()]);

    let empty = headless.empty_point(all);
    headless.click(empty);
    assert!(headless.app.view.selected.is_empty(), "empty canvas clears");
}

#[test]
fn the_checkbox_completes_a_note_that_stays_visible() {
    let mut headless = Headless::new("app-checkbox");
    add_note(&mut headless.app, "task");
    headless.run_frame(Vec::new());
    let id = headless.note_ids()[0].clone();
    let checkbox = headless
        .app
        .view
        .checkbox_center_for(&headless.app.board, &id, headless.app.canvas_rect)
        .expect("checkbox");

    headless.click(checkbox);
    let note = headless.app.board.note(&id).expect("note");
    assert_eq!(note.status, Status::Done);
    assert!(
        !headless.app.view.is_editing(),
        "the checkbox does not edit"
    );
    assert!(
        headless
            .app
            .view
            .screen_rect_for(&headless.app.board, &id, headless.app.canvas_rect)
            .is_some(),
        "a done note stays on the board"
    );

    headless.click(checkbox);
    assert_eq!(
        headless.app.board.note(&id).expect("note").status,
        Status::Open
    );
}

#[test]
fn ctrl_enter_toggles_done_on_the_selection() {
    let mut headless = Headless::new("app-ctrl-enter");
    add_note(&mut headless.app, "a");
    add_note(&mut headless.app, "b");
    headless.run_frame(Vec::new());
    headless.key_with(Key::A, CTRL);
    headless.key_with(Key::Enter, CTRL);
    assert!(
        headless
            .app
            .board
            .notes
            .iter()
            .all(|note| note.status == Status::Done)
    );
    assert!(!headless.app.view.is_editing(), "Ctrl+Enter is not Enter");
    headless.key_with(Key::Enter, CTRL);
    assert!(
        headless
            .app
            .board
            .notes
            .iter()
            .all(|note| note.status == Status::Open)
    );
}

#[test]
fn hide_done_removes_done_notes_and_is_saved() {
    let mut headless = Headless::new("app-hide-done");
    add_note(&mut headless.app, "task");
    headless.run_frame(Vec::new());
    let id = headless.note_ids()[0].clone();
    headless
        .app
        .apply_commands(vec![Command::Complete { id: id.clone() }], "test");
    headless.app.set_hide_done(true);
    headless.run_frame(Vec::new());
    assert!(headless.app.view.selected.is_empty());
    headless.key_with(Key::A, CTRL);
    assert!(
        headless.app.view.selected.is_empty(),
        "hidden notes cannot be selected"
    );
    let (config, _) = Config::load_for(&headless.app.paths);
    assert!(config.hide_done);
}

#[test]
fn an_invalid_config_falls_back_to_defaults() {
    let paths = StorePaths::from_home(scratch_dir("app-config"));
    std::fs::create_dir_all(paths.home()).expect("home");
    std::fs::write(paths.home().join("config.json"), "{ not json").expect("bad config");
    let headless = Headless::with_paths(paths);
    assert_eq!(headless.app.config, Config::default());
}

#[test]
fn an_unopenable_board_shows_an_error_screen_instead_of_panicking() {
    let root = scratch_dir("app-broken");
    let blocker = root.join("home");
    std::fs::write(&blocker, b"a file where the home folder should be").expect("blocker");
    let paths = StorePaths::from_home(&blocker);
    let ctx = Context::default();
    theme::apply(&ctx);
    let app = RustboardApp::for_test(paths, ctx.clone());
    assert!(app.store.is_none());
    assert!(app.store_error.is_some());
}
