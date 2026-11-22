# 170 — Integration, legacy removal, hardening

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** make the real app use everything built so far: SQLite read models, commands, undo,
composer, chat, reminders, tray/hotkey, toasts. Delete the RON persistence path. Ship a green,
signed build.

**Depends on:** all previous tasks. **Owns:** `src/app.rs`, `src/main.rs`, `src/config.rs`,
`src/headless_tests.rs`, `README.md`, `scripts/build.ps1`; deletes `src/persist.rs` and
`src/board.rs`; touches `Cargo.toml` to drop `ron`.

**Read first:** every module's public API; `scripts/build.ps1`; `src/headless_tests.rs`.

## Steps

1. `config.rs`: `config.json` in the board home with `hotkey`, `board_db`, `tz`, `pi_model`,
   `pi_profile`, `reminder_minutes`. Defaults per [000-index](000-index.md). Invalid config
   falls back to defaults with a logged warning, never a panic.
2. Rewire `RustboardApp`: hold `Store`, `model::Board`, `layout`, `ComposerState`, `ChatState`,
   `ReminderEngine`, `Platform`, `Notifier`. Load the board on start; if the DB cannot open,
   show an error panel with the path and a Retry button instead of crashing.
3. Replace the old `board`/`camera` persistence: delete `src/persist.rs`, the eframe
   `rustboard.state.v1` key/migration, `src/board.rs` (old model), and the `ron` dependency.
   Leave eframe persistence for window geometry only, or disable it. A leftover `board.ron` is
   ignored with no code path.
4. `data_version` polling: every 100 ms visible / 1 s hidden, compare
   `Store::data_version()`; on change reload the read model. Defer while a note editor has
   unsaved keystrokes and show a "changed on disk" chip. After our own writes, update the cached
   value so we do not reload ourselves.
5. Toolbar: remove the Delete button; add Undo/Redo, "Open board folder", settings (hotkey,
   profile, test notification), and the board path. Keep it compact. Wire `Del`, `Ctrl+Z`,
   `Ctrl+Shift+Z`, `Ctrl+N`, and `Esc` (edit first, then hide).
6. Wire `ui::composer::show`, `ui::chat::show`, `platform::start`, `notify::ToastNotifier`,
   `remind::start`; focus the composer on the platform `Show` event. Map agent events to the
   chat panel. "Undo this turn" uses the recorded batch range.
7. Update `README.md`: SQLite location, Pi install/auth, board/power modes, hotkey, tray, toasts,
   no email/ingest. Update `scripts/build.ps1` to build and sign all three binaries
   (`rustboard`, `rustboard-mcp`, `todo`).
8. Tests: headless capture → DB row; external second-connection write → poll reload; focused
   editor defers reload; no Delete button in the toolbar; undo/redo via keyboard; legacy
   `board.ron` ignored; config fallback; `cargo test`/`clippy`/`fmt` clean.

## Acceptance

- A fresh launch creates the DB and an empty board; capture, edit, move, complete, snooze,
  delete/undo all persist across restart.
- External writes (MCP/CLI/SQLite browser) appear without restart.
- Hotkey shows/focuses; Esc/X hide; tray Quit exits; a reminder toasts while hidden.
- No `ron`, `persist.rs`, `rustboard.state.v1`, or delete button remains.
- Release build is signed and the three binaries run.

## Out of scope

New product features, search, installer/AUMID packaging beyond the Start Menu shortcut,
multi-board, sync. Fix only what integration breaks.
