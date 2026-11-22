# 010 — Win32 spike findings

Throwaway crate (not committed): `windows` 0.62.2 + `tauri-winrt-notification` 0.8.1; features
`Win32_Foundation`, `Win32_Graphics_Gdi`, `Win32_Storage_EnhancedStorage`, `Win32_Storage_FileSystem`,
`Win32_System_Com`(+_Com_StructuredStorage, _Variant, _LibraryLoader, _Threading),
`Win32_UI_Input_KeyboardAndMouse`, `Win32_UI_Shell`(+_PropertiesSystem), `Win32_UI_WindowsAndMessaging`.
Machine: Windows 11 build 26300.9974, x64, rustc/cargo 1.100.0, eframe/egui 0.37.0, winit 0.30.13.

**Verdict:** hotkey registration+delivery, tray add/rect/double-click dispatch, X hide, AUMID toast and
focus restore were **machine-verified** (synthetic input where a human would act). Real mouse, physical
Esc key, real toast click and hidden-eframe restore in the shipped app remain **manual**.

## Hotkey + restore/focus

```rust
let hwnd = CreateWindowExW(.., Some(HWND_MESSAGE), ..)?;             // dedicated Win32 thread
RegisterHotKey(Some(hwnd), 1, MOD_CONTROL, VK_SPACE.0)?;            // Ctrl+Space -> Ok
RegisterHotKey(Some(hwnd), 2, MOD_CONTROL | MOD_ALT, VK_SPACE.0)?;  // Ctrl+Alt+Space -> Ok
// WM_HOTKEY -> channel event + egui_ctx.request_repaint()
ShowWindow(fg, if IsIconic(fg) { SW_RESTORE } else { SW_SHOW }); SetForegroundWindow(fg);
```

`SendInput` Ctrl+Space and Ctrl+Alt+Space produced `WM_HOTKEY` id 1/2 while Notepad was foreground;
the window became visible + foreground each time, including from minimized. **Plain
`SetForegroundWindow` worked from the handler**; `AttachThreadInput`+`BringWindowToTop` fallback was
built but never needed. Show/restore first, focus second. eframe's `ViewportCommand::Focus` calls winit
`focus_window()`, which does the same Alt-tap + SetForegroundWindow.

## Tray

```rust
NOTIFYICONDATAW { cbSize, hWnd, uID: 1, uFlags: NIF_MESSAGE|NIF_ICON|NIF_TIP,
                  uCallbackMessage: WM_APP+1, hIcon, szTip }; Shell_NotifyIconW(NIM_ADD, &nid);
// lParam low word: WM_LBUTTONDBLCLK -> restore; WM_RBUTTONUP -> SetForegroundWindow(hWnd);
// TrackPopupMenu(TPM_RETURNCMD); PostMessage(hWnd, WM_NULL, 0, 0); dispatch WM_COMMAND
```

`NIM_ADD`=TRUE; `Shell_NotifyIconGetRect` returned a valid rect (2278,1380–2318,1440 physical, 125%
scaling) and still worked while hidden. Double-click dispatch restored the hidden window. Menu: Show /
New note / separator / Quit (4 items); New note and Quit `WM_COMMAND` routing verified; Quit exited
the loop/process.

## Hide on X / Esc (eframe 0.37)

```rust
fn handle_close(ctx: &egui::Context) {
    if ctx.input(|i| i.viewport().close_requested()) {            // ViewportInfo::close_requested()
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose); // same frame or eframe exits
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}
impl eframe::App for RustboardApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) { // runs while hidden
        if self.platform.take_restore_request() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        handle_close(ctx); // close can arrive while hidden
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) { handle_close(ui.ctx()); }
}
```

Headless run of this shape: `CLOSE_SEEN=true CANCEL_CLOSE_EMITTED=true` (eframe checks CancelClose in
the same frame, `epi_integration.rs:366-370`; logic-only while hidden, `wgpu_integration.rs:716-751`).
eframe runs **no `ui` pass while hidden** and `logic` only after a repaint — the hotkey thread must hold
a cloned `egui::Context` and call `request_repaint()` after its event. Esc, only with no editor focused:

```rust
if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !ctx.egui_wants_keyboard_input() {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)); }
```

Win32 proof for X: `WM_CLOSE` -> `ShowWindow(SW_HIDE)`; process stayed alive and the hotkey restored it.
Do not send `CancelClose` for Esc.

## AUMID and toast

- `SetCurrentProcessExplicitAppUserModelID(L"com.rustboard.app")` -> `Ok`.
- `IShellLinkW` + `IPropertyStore` (`PKEY_AppUserModel_ID`) + `IPersistFile::Save` created
  `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Rustboard Spike.lnk`; readback
  `com.rustboard.app` (match=true). **Name it `Rustboard.lnk`** so the toast header reads "Rustboard".
- Also created `HKCU\Software\Classes\AppUserModelId\com.rustboard.app` (`DisplayName=Rustboard`),
  the registry fallback for unpackaged apps; the confirmed toast had both registrations.
- `Toast::new("com.rustboard.app").title("Rustboard").text1(..).duration(Short).sound(None).show()`
  -> `Ok(())` with no app window. Screenshot captured the banner bottom-right, and Windows created
  `HKCU\...\Notifications\Settings\com.rustboard.app` (`LastNotificationAddedTime`,
  `PeriodicNotificationCount=1`).
- Caveat: a shortcut-only retest also returned `Ok` and recorded the notification, but no banner was
  captured (foreground was a full-window browser; fullscreen/quiet states suppress banners). Keep the
  registry key and verify in the real install: **`show()==Ok` is not proof of display**.
- **Click-to-focus not proven**: a click launches the shortcut (new process) unless a single-instance
  forwarder and/or `PKEY_AppUserModel_ToastActivatorCLSID` + COM activator exists (160/170).
- Fallback shape if toasts are disabled: `ViewportCommand::RequestUserAttention(Informational)` + an
  in-app banner. Not needed here.

## Failure modes / manual checks

- `RegisterHotKey` fails 1409 if another app owns the combo; try Ctrl+Alt+Space, surface the error.
- Register the AUMID at install/first run, not immediately before the first toast; wait for the shell
  (3 s was enough).
- Hidden eframe = no `ui`; without `request_repaint` the tray/hotkey cannot reach it.
- Tray rect is physical pixels (125% scaling); screenshots are logical — do not compare directly.
- Manual: physical Ctrl+Space from another app; hidden-eframe -> hotkey restore; real tray left/right
  click and every menu item; Esc with an editor focused (no hide) vs canvas (hide); toast click-to-focus.
- Cleanup: `.lnk`, AUMID key, per-app notification entry and tray icon removed; scratch crate and
  screenshots deleted; only this file committed.
