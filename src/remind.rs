//! Reminder engine (task 140).
//!
//! A dedicated thread polls the board for reminders that are due, marks each
//! note reminded inside one transaction (system batch + event, committed
//! before any notification), and hands the notification to a ReminderSink.
//! Task 150's toast notifier and task 170's app implement the sink; nothing
//! here depends on WinRT or egui.
//!
//! Snooze, done and deleted notes are respected. Reminders older than the
//! missed window are collapsed into one summary notification on the first tick
//! so a week away does not produce a week of popups.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::json;

use crate::model::{self, Note};
use crate::store::{Store, StoreError, StorePaths, now_utc_ms, schema};

/// How often the engine looks for due reminders.
pub const REMINDER_INTERVAL: Duration = Duration::from_secs(30);

/// Reminders older than this are summarised instead of notified one by one.
pub const MISSED_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// One notification the app should show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderNotice {
    pub title: String,
    pub body: String,
    /// Stable tag for the OS notification (the note id, or "missed").
    pub tag: String,
    pub urgent: bool,
}

/// Where reminders go. Implemented by task 150's ToastNotifier (via task 170)
/// and by the tests with a recording sink.
pub trait ReminderSink: Send + Sync {
    fn notify(&self, notice: ReminderNotice);
}

/// What one tick did, for tests and logging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Notes marked reminded and notified individually this tick.
    pub fired: Vec<String>,
    /// Notes collapsed into the missed summary.
    pub missed: usize,
}

/// Errors from the reminder engine.
#[derive(Debug, thiserror::Error)]
pub enum RemindError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Model(#[from] model::ModelError),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error("cannot encode a reminder row: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cannot start the reminder thread: {0}")]
    Thread(#[from] std::io::Error),

    #[error("cannot start the reminder engine: {0}")]
    Startup(String),
}

/// The testable core: one connection, no thread and no sleeps.
pub struct ReminderCore {
    store: Store,
    first_tick: bool,
}

impl ReminderCore {
    /// Open the board for reminders (this runs migrations like any writer).
    pub fn open(paths: &StorePaths) -> Result<Self, RemindError> {
        let store = schema::open_board(paths)?;
        Ok(Self {
            store,
            first_tick: true,
        })
    }

    /// Run one pass at the given instant.
    ///
    /// On the first tick, reminders older than MISSED_WINDOW_MS are marked and
    /// summarised in one notification. Everything else that is due is marked
    /// and notified individually, oldest first.
    pub fn tick(&mut self, now: i64, sink: &dyn ReminderSink) -> Result<TickReport, RemindError> {
        let due = due_reminders(self.store.connection(), now)?;
        let mut report = TickReport::default();

        let (missed, fresh): (Vec<Note>, Vec<Note>) = if self.first_tick {
            due.into_iter()
                .partition(|note| remind_instant(note).unwrap_or(i64::MAX) < now - MISSED_WINDOW_MS)
        } else {
            (Vec::new(), due)
        };
        self.first_tick = false;

        for note in missed {
            self.mark_reminded(&note, now)?;
            report.missed += 1;
        }
        if report.missed > 0 {
            sink.notify(ReminderNotice {
                title: "Rustboard".to_owned(),
                body: format!(
                    "{} reminder{} while you were away",
                    report.missed,
                    if report.missed == 1 { "" } else { "s" }
                ),
                tag: "missed".to_owned(),
                urgent: false,
            });
        }

        for note in fresh {
            self.mark_reminded(&note, now)?;
            report.fired.push(note.id.clone());
            sink.notify(ReminderNotice {
                title: "Reminder".to_owned(),
                body: if note.text.trim().is_empty() {
                    "A note is due".to_owned()
                } else {
                    note.text.clone()
                },
                tag: note.id.clone(),
                urgent: note.important,
            });
        }

        Ok(report)
    }

    /// Writes the reminded marker plus a system batch/event, in one
    /// transaction, and commits before the caller notifies.
    fn mark_reminded(&mut self, note: &Note, now: i64) -> Result<(), RemindError> {
        let batch_id = uuid::Uuid::new_v4().to_string();
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
             VALUES (?1, ?2, 'system', NULL, 'reminder', 'apply')",
            params![batch_id, now],
        )?;
        tx.execute(
            "UPDATE notes SET reminded_at_utc_ms = ?1
              WHERE id = ?2 AND reminded_at_utc_ms IS NULL",
            params![now, note.id],
        )?;
        let after = model::load_note(&tx, &note.id)?;
        let before_json = serde_json::to_string(note)?;
        let after_json = match &after {
            Some(after) => serde_json::to_string(after)?,
            None => "null".to_owned(),
        };
        tx.execute(
            "INSERT INTO events
                 (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
                  payload_json, before_json, after_json)
             VALUES (?1, 0, ?2, 'system', 'remind', 'note', ?3, ?4, ?5, ?6)",
            params![
                batch_id,
                now,
                note.id,
                json!({ "command": "remind", "note_id": note.id }).to_string(),
                before_json,
                after_json
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Due reminders: active, open, reminded not yet fired, snooze elapsed.
fn due_reminders(conn: &Connection, now: i64) -> Result<Vec<Note>, RemindError> {
    let mut statement = conn.prepare(
        "SELECT id FROM notes
          WHERE deleted_at_utc_ms IS NULL
            AND status = 'open'
            AND remind_utc_ms IS NOT NULL
            AND remind_utc_ms <= ?1
            AND reminded_at_utc_ms IS NULL
            AND (snooze_utc_ms IS NULL OR snooze_utc_ms <= ?1)
          ORDER BY remind_utc_ms, id",
    )?;
    let ids = statement.query_map([now], |row| row.get::<_, String>(0))?;
    let mut notes = Vec::new();
    for id in ids {
        if let Some(note) = model::load_note(conn, &id?)? {
            notes.push(note);
        }
    }
    Ok(notes)
}

/// The reminder instant of a loaded note.
fn remind_instant(note: &Note) -> Option<i64> {
    note.remind
        .as_ref()
        .and_then(|spec| spec.resolve().ok())
        .map(|resolved| resolved.utc_ms)
}

/// A running reminder thread.
pub struct ReminderEngine {
    stop: Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl ReminderEngine {
    /// Start the background thread; it ticks once immediately and then every
    /// interval.
    pub fn start(
        paths: StorePaths,
        sink: Arc<dyn ReminderSink>,
        interval: Duration,
    ) -> Result<Self, RemindError> {
        let (stop, stopped): (Sender<()>, Receiver<()>) = channel();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("rustboard-reminders".to_owned())
            .spawn(move || {
                let mut core = match ReminderCore::open(&paths) {
                    Ok(core) => core,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));

                if let Err(error) = core.tick(now_utc_ms(), sink.as_ref()) {
                    eprintln!("rustboard: reminder pass failed: {error}");
                }

                loop {
                    match stopped.recv_timeout(interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(error) = core.tick(now_utc_ms(), sink.as_ref()) {
                                eprintln!("rustboard: reminder pass failed: {error}");
                            }
                        }
                    }
                }
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                join: Some(join),
            }),
            Ok(Err(message)) => {
                let _ = join.join();
                Err(RemindError::Startup(message))
            }
            Err(_) => {
                let _ = join.join();
                Err(RemindError::Startup(
                    "reminder thread exited before it was ready".to_owned(),
                ))
            }
        }
    }

    /// Ask the thread to stop and wait for it. Idempotent.
    pub fn stop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ReminderEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::command::{Actor, Command};
    use crate::store::scratch_dir;
    use crate::time::TimeSpec;

    #[derive(Default)]
    struct RecordingSink {
        notices: Mutex<Vec<ReminderNotice>>,
    }

    impl ReminderSink for RecordingSink {
        fn notify(&self, notice: ReminderNotice) {
            self.notices.lock().expect("sink lock").push(notice);
        }
    }

    impl RecordingSink {
        fn notices(&self) -> Vec<ReminderNotice> {
            self.notices.lock().expect("sink lock").clone()
        }
    }

    fn paths(name: &str) -> StorePaths {
        StorePaths::from_home(scratch_dir(name))
    }

    fn at(local: &str) -> i64 {
        TimeSpec {
            local: local.to_owned(),
            tz: "Europe/London".to_owned(),
        }
        .resolve()
        .expect("instant")
        .utc_ms
    }

    /// Adds a note with a reminder via the command layer.
    fn add_reminder(store: &mut Store, text: &str, remind_local: &str) -> String {
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: text.to_owned(),
                    container: None,
                    at_root: true,
                    parent: None,
                    due: None,
                    remind: Some(TimeSpec {
                        local: remind_local.to_owned(),
                        tz: "Europe/London".to_owned(),
                    }),
                    important: false,
                    source: "test".to_owned(),
                }],
                Actor::Human,
                Some("test"),
            )
            .expect("add reminder");
        applied.new_ids.notes[0].clone()
    }

    #[test]
    fn one_notification_per_due_reminder_and_test_the_boundary() {
        let paths = paths("remind-one");
        let mut store = schema::open_board(&paths).expect("open");
        add_reminder(&mut store, "pay rent", "2026-11-22T09:00:00");
        add_reminder(&mut store, "later", "2026-11-22T11:00:00");

        let sink = RecordingSink::default();
        let mut core = ReminderCore::open(&paths).expect("core");
        let report = core.tick(at("2026-11-22T10:00:00"), &sink).expect("tick");

        assert_eq!(report.fired.len(), 1);
        assert_eq!(report.missed, 0);
        assert_eq!(sink.notices().len(), 1);
        assert_eq!(sink.notices()[0].body, "pay rent");

        // Exactly at the reminder instant counts as due.
        let report = core.tick(at("2026-11-22T11:00:00"), &sink).expect("tick");
        assert_eq!(report.fired.len(), 1);
        assert_eq!(sink.notices().len(), 2);

        // No duplicate on the next pass or after a restart.
        let report = core.tick(at("2026-11-22T12:00:00"), &sink).expect("tick");
        assert!(report.fired.is_empty());
        drop(core);
        let mut restarted = ReminderCore::open(&paths).expect("reopen");
        restarted
            .tick(at("2026-11-22T12:00:00"), &sink)
            .expect("tick");
        assert_eq!(sink.notices().len(), 2);
    }

    #[test]
    fn snooze_suppresses_until_it_expires_and_done_never_fires() {
        let paths = paths("remind-snooze");
        let mut store = schema::open_board(&paths).expect("open");
        let snoozed = add_reminder(&mut store, "snoozed", "2026-11-22T09:00:00");
        let done = add_reminder(&mut store, "done", "2026-11-22T09:00:00");
        let snooze_until = TimeSpec {
            local: "2026-11-22T10:00:00".to_owned(),
            tz: "Europe/London".to_owned(),
        };
        store
            .apply(
                &[
                    Command::Snooze {
                        id: snoozed.clone(),
                        until: snooze_until,
                    },
                    Command::Complete { id: done },
                ],
                Actor::Human,
                None,
            )
            .expect("snooze and complete");

        let sink = RecordingSink::default();
        let mut core = ReminderCore::open(&paths).expect("core");
        core.tick(at("2026-11-22T09:30:00"), &sink).expect("tick");
        assert!(sink.notices().is_empty(), "snoozed and done stay quiet");

        core.tick(at("2026-11-22T10:00:00"), &sink).expect("tick");
        let notices = sink.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].body, "snoozed");
    }

    #[test]
    fn reopening_rearms_a_reminder() {
        let paths = paths("remind-reopen");
        let mut store = schema::open_board(&paths).expect("open");
        let id = add_reminder(&mut store, "rearm", "2026-11-22T09:00:00");
        let sink = RecordingSink::default();
        let mut core = ReminderCore::open(&paths).expect("core");
        core.tick(at("2026-11-22T09:00:00"), &sink).expect("tick");
        assert_eq!(sink.notices().len(), 1);

        store
            .apply(&[Command::Complete { id: id.clone() }], Actor::Human, None)
            .expect("complete");
        store
            .apply(&[Command::Reopen { id: id.clone() }], Actor::Human, None)
            .expect("reopen");

        core.tick(at("2026-11-22T09:01:00"), &sink).expect("tick");
        assert_eq!(sink.notices().len(), 2, "reopen clears the fired marker");
    }

    #[test]
    fn old_reminders_become_one_summary() {
        let paths = paths("remind-missed");
        let mut store = schema::open_board(&paths).expect("open");
        for index in 0..3 {
            add_reminder(
                &mut store,
                &format!("missed {index}"),
                "2026-11-20T09:00:00",
            );
        }
        add_reminder(&mut store, "fresh", "2026-11-22T09:00:00");

        let sink = RecordingSink::default();
        let mut core = ReminderCore::open(&paths).expect("core");
        let report = core.tick(at("2026-11-22T10:00:00"), &sink).expect("tick");

        assert_eq!(report.missed, 3);
        assert_eq!(report.fired.len(), 1);
        let notices = sink.notices();
        assert_eq!(notices.len(), 2, "one summary plus one fresh reminder");
        assert!(notices[0].body.contains("3 reminders"));
        assert_eq!(notices[1].body, "fresh");
    }

    #[test]
    fn every_fired_reminder_writes_a_system_batch_and_event() {
        let paths = paths("remind-audit");
        let mut store = schema::open_board(&paths).expect("open");
        add_reminder(&mut store, "audit me", "2026-11-22T09:00:00");
        let sink = RecordingSink::default();
        let mut core = ReminderCore::open(&paths).expect("core");
        core.tick(at("2026-11-22T09:00:00"), &sink).expect("tick");

        let (batches, _, events, _) =
            crate::command::audit_counts(store.connection()).expect("counts");
        assert_eq!(batches, 2, "add batch plus reminder batch");
        assert_eq!(events, 2);
        let system: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE actor = 'system'",
                [],
                |row| row.get(0),
            )
            .expect("system batches");
        assert_eq!(system, 1);
        let reminded: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE reminded_at_utc_ms IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("reminded");
        assert_eq!(reminded, 1);
    }

    #[test]
    fn stop_is_idempotent_and_the_thread_exits() {
        let paths = paths("remind-thread");
        let sink = Arc::new(RecordingSink::default());
        let mut engine =
            ReminderEngine::start(paths, sink, Duration::from_millis(10)).expect("engine starts");
        engine.stop();
        engine.stop();
    }
}
