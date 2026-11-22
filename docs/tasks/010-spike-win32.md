# 010 — Spike: Win32 shell primitives in eframe

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** prove the four Windows behaviours the app depends on, on this machine, and document
the exact code shape. Throwaway code is fine; the findings are the deliverable.

**Depends on:** nothing. **Parallel with:** 005, 020.
**Owns:** `docs/tasks/010-findings.md`, a scratch branch/file you delete afterwards. Do not
touch production modules.

**Read first:** `src/main.rs` (eframe 0.37 options), `src/app.rs` (panel structure).
Useful APIs: `eframe::Frame::winit_window()`, `egui::ViewportCommand::{Visible, Minimized,
Focus, CancelClose}`, winit's `focus_window`/`set_visible`.

## Steps

1. **Hotkey.** Start a message-only window on a dedicated thread and call `RegisterHotKey` for
   `Ctrl+Space` (and `Ctrl+Alt+Space` as a fallback). On `WM_HOTKEY`, restore and focus the
   eframe window. Test from another app and from a minimized window. If focus is refused, try
   `AttachThreadInput` + `SetForegroundWindow` and record what worked.
2. **Tray.** `Shell_NotifyIconW` with an `.ico`, tooltip, `WM_APP` callback, right-click
   `TrackPopupMenu` (Show / New note / Quit), double-click to show. Confirm the icon survives
   the window being hidden and that Quit exits the process.
3. **Hide on X / Esc.** Intercept `ViewportInfo::close_requested`, send `CancelClose` +
   `Visible(false)`. For Esc, hide only when no text editor has focus. Confirm the process keeps
   running and the hotkey can bring it back.
4. **Toast.** Register AUMID `com.rustboard.app` with a Start Menu shortcut (`IShellLinkW` +
   `IPropertyStore`), call `SetCurrentProcessExplicitAppUserModelID`, and show a toast with
   `tauri-winrt-notification`. Test while the app is hidden. Record whether the shortcut is
   required, whether click-to-focus works, and the fallback (taskbar flash + in-app banner).
5. Write `docs/tasks/010-findings.md` (≤1 page): crates/features, exact AUMID/shortcut path,
   hotkey registration result, focus technique, close/Esc interception, toast result, and
   failure modes. Delete the scratch code or leave it on a branch; do not merge half-built code.

## Acceptance

- Hotkey fires while another app is focused, and the window restores and focuses.
- Tray icon, menu and double-click work; Quit exits.
- X and Esc hide to tray without exiting; the hotkey restores.
- A toast with the Rustboard AUMID appears while hidden, or the documented fallback is proven.
- Findings are one page and reproducible.

## Out of scope

Production `platform` module, settings UI, configurable hotkeys, reminder scheduling, installer
packaging. Do not add the crates to `Cargo.toml` on `main`; the spike branch can carry them.
