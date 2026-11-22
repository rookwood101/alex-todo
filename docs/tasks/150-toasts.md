# 150 — Windows toast notifications

**Goal:** real Windows toasts for reminders, with a documented fallback. One `Notifier` trait so
140 and 170 never depend on WinRT directly.

**Depends on:** 010 (spike). **Parallel with:** 140, 160.
**Owns:** `src/notify.rs`, `assets/rustboard.ico` (or reuse an existing icon), toast tests. Add
`tauri-winrt-notification` to `Cargo.toml` in this PR.

**Read first:** `docs/tasks/010-findings.md`; this index; `src/main.rs` (app id, exe path).

## Steps

1. `Notifier` trait: `notify(Notification { title, body, tag, urgent }) -> Result<(), NotifyError>`
   and `test()`. `Notification` is plain data; no UI types.
2. `ToastNotifier`:
   - On first use, ensure AUMID `com.rustboard.app` is registered by creating
     `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Rustboard.lnk` with the exe as target and
     the `System.AppUserModel.ID` property (`IShellLinkW` + `IPropertyStore`). Idempotent.
   - Call `SetCurrentProcessExplicitAppUserModelID` once at startup.
   - Show with `tauri-winrt-notification` (`Toast::new("com.rustboard.app")`).
   - Click focuses the app if the OS supports it; otherwise dismiss-only is acceptable and
     documented. No action buttons in v1.
3. Fallback: if registration or `show()` fails, use taskbar flash
   (`winit::Window::request_user_attention`) plus an in-app banner via an event channel. Log the
   failure; never panic.
4. Settings/debug: expose `test()` and a `--test-notification` flag (or a toolbar button wired
   by 170) so a human can verify Focus Assist/notification settings.
5. Tests: shortcut path/idempotence; AUMID string; fallback backend is called when the toast
   backend returns an error (inject a `ToastBackend` trait); no blocking on the UI thread.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- A toast appears while the app is hidden on this machine, or the fallback is observed and
  documented.
- No secrets, no installer requirement beyond the Start Menu shortcut.

## Out of scope

Reminder scheduling (140), tray/hotkey (160), action buttons, history/notification center,
installer packaging. If toasts genuinely cannot work unpublished, keep the fallback and say so
in the findings.
