# 140 — Reminder thread

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** fire reminders while the app is visible or hidden, once per reminder, respecting
snooze and done. Uses a notifier; does not implement toasts.

**Depends on:** 050 (system batches), 060 (time). **Parallel with:** 150/160.
**Owns:** `src/remind.rs`, `src/remind` module declaration.

**Read first:** `src/time.rs`; `src/store/**`. Define a one-method
`ReminderSink { fn notify(&self, ReminderNotice) }` in this module so the task does not depend on
150; 170 adapts 150's `Notifier` to it.

## Steps

1. `ReminderEngine` owning a dedicated thread with its own SQLite connection (WAL allows it).
   Every 30 s (and immediately on start) select due notes:
   `deleted_at IS NULL AND status='open' AND remind_utc_ms IS NOT NULL AND remind_utc_ms <= now
   AND reminded_at_utc_ms IS NULL AND (snooze_utc_ms IS NULL OR snooze_utc_ms <= now)`.
2. For each due note, in one transaction: set `reminded_at_utc_ms`, append a system batch/event
   (`actor='system'`, `kind='apply'`), commit, then call
   `ReminderSink::notify`. Commit before notifying so a crash cannot double-fire.
3. Snooze: a snoozed note is skipped until `snooze_utc_ms` passes; then its reminder fires if
   due. Done notes never fire. Reopening re-arms only if `reminded_at` is cleared by `reopen`
   (040 must do that; verify).
4. Missed reminders: on startup, collect due reminders older than 24 h and send **one** summary
   notification (`"3 reminders while you were away"`), not N popups.
5. Lifecycle: start on app start, stop cleanly on exit, expose a `tick()` for tests with an
   injected clock; no `sleep` in tests.
6. Tests: fake clock + temp DB: one notification per reminder; snooze suppresses until expiry;
   done suppresses; missed summary counts; reminder exactly at the boundary; no duplicate on
   restart; system batch/event written.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- No duplicate notifications across restarts or concurrent ticks.
- The app can show/hide without affecting reminders.
- Toast failures are logged, not fatal; the reminder is still marked.

## Out of scope

The toast implementation (150), tray (160), smart scheduling, recurrence, calendar. Reminder
interval and window are constants in this task.
