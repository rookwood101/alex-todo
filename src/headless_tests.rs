//! Headless integration tests for the app shell (task 170).
//!
//! Each test drives the real [RustboardApp] with an egui context and a private
//! SQLite board in target/test-tmp. No window, no network and no live model.

use eframe::egui::{Context, Event, Key, Modifiers, Pos2, RawInput, Rect, vec2};

use crate::app::RustboardApp;
use crate::command::{Actor, Command};
use crate::config::Config;
use crate::model::Board;
use crate::store::{StorePaths, schema, scratch_dir};
use crate::theme;

const FRAME_SECONDS: f64 = 1.0 / 60.0;

struct Headless {
    ctx: Context,
    app: RustboardApp,
    screen: Rect,
    time: f64,
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
        };
        headless.run_frame(Vec::new());
        headless
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

    fn key(&mut self, key: Key) {
        self.run_frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]);
    }
}

fn add_note(app: &mut RustboardApp, text: &str) {
    app.apply_commands(
        vec![Command::AddNote {
            text: text.to_owned(),
            group: None,
            parent: None,
            due: None,
            remind: None,
            priority: None,
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
                group: None,
                parent: None,
                due: None,
                remind: None,
                priority: None,
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

    assert_eq!(
        headless.app.board.notes.len(),
        1,
        "composer Enter creates one note"
    );
    let note = &headless.app.board.notes[0];
    assert_eq!(note.text, "buy milk");
    assert!(note.due.is_some(), "capture chips were applied");

    let stored = Board::load(headless.app.store.as_ref().expect("store").connection())
        .expect("stored board");
    assert_eq!(stored.notes.len(), 1);
    assert_eq!(stored.notes[0].text, "buy milk");
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
fn delete_on_a_selected_note_does_not_delete_it() {
    let mut headless = Headless::new("app-no-delete");
    add_note(&mut headless.app, "keep me");
    let id = headless.app.board.notes[0].id.clone();
    headless.app.view.selected = Some(id);
    headless.run_frame(Vec::new());
    headless.key(Key::Delete);
    assert_eq!(
        headless.app.board.notes.len(),
        1,
        "there is no delete affordance in the app"
    );
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
