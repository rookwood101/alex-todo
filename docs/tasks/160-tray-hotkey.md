# 160 — Tray, global hotkey, hide-to-tray

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** one Win32 message thread owns the hotkey and tray icon, and the app hides to tray on
X/Esc instead of exiting. This is what makes Rustboard a background app.

**Depends on:** 010 (spike). **Parallel with:** 140, 150.
**Owns:** `src/platform/win32.rs`, `src/platform` module declaration. Add the `windows` and
`crossbeam-channel` deps in this PR.

**Read first:** `docs/tasks/010-findings.md`; `src/main.rs`; `src/app.rs` viewport
handling.

## Steps

1. `Platform::start(config) -> PlatformHandle` spawns a thread with a message-only window:
   - `RegisterHotKey` for the configured chord (default **`Ctrl+Space`**, two keys); on
     `WM_HOTKEY` post `Show` to the app; if registration fails, return a typed error and let
     170 fall back to `Ctrl+Alt+Space`.
   - `Shell_NotifyIconW` tray icon (`assets/rustboard.ico`), tooltip, `WM_APP` callback,
     right-click `TrackPopupMenu` (Show / New note / Quit), double-click Show.
   - Show/focus: `ShowWindow(SW_RESTORE)` + `SetForegroundWindow`; if focus is refused, use
     `AttachThreadInput` with the foreground thread and retry.
2. `PlatformEvent { Show, NewNote, Quit, Toggle }` over a channel + `ctx.request_repaint()`.
   The app polls the receiver each frame; Quit exits the process.
3. Hide: expose `request_hide()`; 170 intercepts `ViewportInfo::close_requested` with
   `CancelClose` + `Visible(false)` and calls `request_hide()` on Esc when no editor is focused.
   Show a one-time "still running in the tray" balloon.
4. Single instance: named mutex; a second launch finds the tray window class, posts `Show`, and
   exits 0. The first instance stays.
5. Tests: hotkey chord parsing/validation; start/stop is idempotent; a second `Platform::start`
   in-process is rejected. OS interactions are manual acceptance.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- Manual: hotkey while unfocused, tray menu, double-click, X and Esc hide, second launch focuses
  the first and exits.
- No orphan thread/window after quit; no duplicate hotkey registration.

## Out of scope

Toasts (150), reminder scheduling (140), settings UI for the chord (170), start-at-login,
installer, non-Windows platforms. Keep the module `#[cfg(windows)]`.
