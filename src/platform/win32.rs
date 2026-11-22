//! Win32 implementation of the platform module (task 160).
//!
//! One dedicated thread owns a message-only window, the global hotkey and the
//! notification-area icon. All OS handles are created, used and destroyed on
//! that thread. The public [`PlatformHandle`] talks to it through channels and
//! wakes the blocking message loop with [`PostThreadMessageW`].
//!
//! Messages used by the window:
//!
//! - [`WM_HOTKEY`]: show the target, emit [`PlatformEvent::Show`] and repaint;
//! - [`WM_APP + 1`]: tray callback (double-click shows, right-click opens the menu);
//! - [`WM_APP + 2`]: a second launch asking this instance to show itself;
//! - [`WM_APP + 3`]: command wake-up (hide/stop).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, RecvTimeoutError, Sender as StdSender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    SetFocus, UnregisterHotKey, VK_0, VK_A, VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowExW, FindWindowW,
    GCLP_HICONSM, GWLP_USERDATA, GetClassLongPtrW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowThreadProcessId, HCURSOR, HICON, HWND_MESSAGE,
    ICON_BIG, ICON_SMALL, IDI_APPLICATION, IMAGE_ICON, IsIconic, IsWindow, LR_LOADFROMFILE,
    LoadIconW, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    PostThreadMessageW, RegisterClassW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_RESTORE, SW_SHOW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_GETICON, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY,
    WM_NULL, WM_QUIT, WM_RBUTTONUP, WNDCLASS_STYLES, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use super::{Hotkey, HotkeyKey, PlatformConfig, PlatformError, PlatformEvent, PlatformStart};

const HOTKEY_PRIMARY_ID: i32 = 1;
const HOTKEY_FALLBACK_ID: i32 = 2;
const TRAY_ID: u32 = 1;
const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SHOW: u32 = WM_APP + 2;
const WM_APP_WAKE: u32 = WM_APP + 3;
const MENU_SHOW: usize = 1;
const MENU_NEW_NOTE: usize = 2;
const MENU_QUIT: usize = 3;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const FORWARD_ATTEMPTS: usize = 20;
const FORWARD_POLL: Duration = Duration::from_millis(100);

/// Entry point for the Win32 platform integration.
pub struct Platform;

impl Platform {
    /// Spawn the platform thread and register the hotkey and tray icon.
    ///
    /// Returns [`PlatformStart::ForwardedToExisting`] if another process already
    /// owns `Local\Rustboard.SingleInstance`; in that case this process should
    /// exit with code 0. Otherwise the returned handle owns the thread until it
    /// is stopped or dropped.
    ///
    /// `repaint` is called after every event and on every Show request. eframe
    /// only runs `App::logic` after a repaint, so this hook is what makes the
    /// hidden app reachable from the hotkey and tray.
    ///
    /// # Errors
    ///
    /// Returns a [`PlatformError`] if the message window cannot be created, both
    /// hotkey chords are rejected, the tray icon cannot be added (when
    /// [`PlatformConfig::tray_icon`] is set), or the thread fails to start.
    pub fn start(
        config: PlatformConfig,
        repaint: impl Fn() + Send + 'static,
    ) -> Result<PlatformStart, PlatformError> {
        let mut instance = SingleInstance::acquire()?;
        if !instance.is_first() {
            instance.close();
            forward_show_to_existing();
            return Ok(PlatformStart::ForwardedToExisting);
        }

        let (events_tx, events_rx) = unbounded();
        let (commands_tx, commands_rx) = unbounded();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<u32>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Hotkey, PlatformError>>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let target = Arc::new(AtomicIsize::new(0));

        let args = ThreadArgs {
            config,
            repaint: Box::new(repaint),
            events: events_tx,
            commands: commands_rx,
            target: Arc::clone(&target),
            started: started_tx,
            ready: ready_tx,
            done: done_tx,
        };

        let thread = thread::Builder::new()
            .name(String::from("rustboard-platform"))
            .spawn(move || run_message_thread(args))
            .map_err(|error| PlatformError::ThreadSpawn(error.to_string()))?;

        let thread_id = match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(thread_id) => thread_id,
            Err(RecvTimeoutError::Timeout) => return Err(PlatformError::StartTimeout),
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                return Err(PlatformError::ThreadStopped);
            }
        };

        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(hotkey)) => Ok(PlatformStart::Started(PlatformHandle {
                events: events_rx,
                target,
                hotkey,
                control: Some(Control {
                    commands: commands_tx,
                    done: done_rx,
                    thread: Some(thread),
                    thread_id,
                    _instance: instance,
                }),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                // Ask the stuck thread to leave its message loop; never block
                // forever waiting for it.
                unsafe {
                    let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                Err(PlatformError::StartTimeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(PlatformError::ThreadStopped)
            }
        }
    }
}

/// A running platform thread.
///
/// Drop the handle (or call [`PlatformHandle::stop`]) to remove the hotkey and tray
/// icon and join the thread.
pub struct PlatformHandle {
    events: Receiver<PlatformEvent>,
    target: Arc<AtomicIsize>,
    hotkey: Hotkey,
    control: Option<Control>,
}

impl PlatformHandle {
    /// Take the next pending event without blocking.
    ///
    /// Returns `None` when the queue is empty. Call this in a loop from
    /// `App::logic` to drain every event for a frame.
    pub fn take_event(&self) -> Option<PlatformEvent> {
        self.events.try_recv().ok()
    }

    /// Ask the platform thread to hide the target window and show the one-time
    /// "still running in the tray" balloon.
    ///
    /// The app should also send `ViewportCommand::Visible(false)`; this call makes
    /// the native window disappear immediately. Calling it repeatedly is safe;
    /// the balloon is only requested once.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Stopped`] if the platform thread has already
    /// stopped.
    pub fn request_hide(&self) -> Result<(), PlatformError> {
        let Some(control) = self.control.as_ref() else {
            return Err(PlatformError::Stopped);
        };
        if control.is_stopped() {
            return Err(PlatformError::Stopped);
        }
        control.request(Command::Hide)
    }

    /// Set the raw `HWND` (as an `isize`) that Show should restore and focus.
    ///
    /// Pass `0` to clear it and fall back to
    /// [`PlatformConfig::window_title`]. Supplying the handle is optional; the
    /// title fallback is enough for a single-window app.
    pub fn set_target_window(&self, hwnd: isize) {
        self.target.store(hwnd, Ordering::SeqCst);
    }

    /// The chord that was actually registered.
    ///
    /// This is [`PlatformConfig::fallback_hotkey`] when the preferred chord was
    /// already owned by another program.
    pub fn hotkey(&self) -> Hotkey {
        self.hotkey
    }

    /// Stop the platform thread and release the hotkey, tray icon and
    /// single-instance mutex.
    ///
    /// Idempotent: calling it twice returns `Ok(())`. [`Drop`] calls it with the
    /// result discarded.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::ShutdownTimeout`] if the thread does not stop in
    /// time.
    pub fn stop(&mut self) -> Result<(), PlatformError> {
        match self.control.as_mut() {
            Some(control) => control.shutdown(),
            None => Ok(()),
        }
    }
}

impl Drop for PlatformHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A command sent from the app thread to the platform thread.
#[derive(Clone, Copy, Debug)]
enum Command {
    Hide,
    Stop,
}

/// The platform thread's side of the handle.
struct Control {
    commands: Sender<Command>,
    done: StdReceiver<()>,
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    _instance: SingleInstance,
}

impl Control {
    fn is_stopped(&self) -> bool {
        self.thread.is_none()
    }

    fn request(&self, command: Command) -> Result<(), PlatformError> {
        self.commands
            .send(command)
            .map_err(|_| PlatformError::Stopped)?;
        self.wake()
    }

    fn wake(&self) -> Result<(), PlatformError> {
        unsafe { PostThreadMessageW(self.thread_id, WM_APP_WAKE, WPARAM(0), LPARAM(0)) }
            .map_err(|_| win32_error("PostThreadMessageW"))
    }

    fn shutdown(&mut self) -> Result<(), PlatformError> {
        if self.thread.is_none() {
            return Ok(());
        }

        let _ = self.commands.send(Command::Stop);
        let _ = self.wake();

        match self.done.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => Err(PlatformError::ShutdownTimeout),
        }
    }
}

/// The named mutex that makes Rustboard a single-instance app.
struct SingleInstance {
    handle: Option<HANDLE>,
    first: bool,
}

impl SingleInstance {
    fn acquire() -> Result<Self, PlatformError> {
        let result = unsafe { CreateMutexW(None, false, mutex_name()) };
        // Capture the "already exists" flag before mapping the result: the
        // error path below calls GetLastError again.
        let first = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
        let handle = result.map_err(|_| win32_error("CreateMutexW"))?;
        Ok(Self {
            handle: Some(handle),
            first,
        })
    }

    fn is_first(&self) -> bool {
        self.first
    }

    fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        self.close();
    }
}

/// Arguments moved into the platform thread.
struct ThreadArgs {
    config: PlatformConfig,
    repaint: Box<dyn Fn() + Send>,
    events: Sender<PlatformEvent>,
    commands: Receiver<Command>,
    target: Arc<AtomicIsize>,
    started: StdSender<u32>,
    ready: StdSender<Result<Hotkey, PlatformError>>,
    done: StdSender<()>,
}

/// Everything the window procedure needs, owned by the platform thread.
struct WindowState {
    hwnd: HWND,
    config: PlatformConfig,
    repaint: Box<dyn Fn() + Send>,
    events: Sender<PlatformEvent>,
    commands: Receiver<Command>,
    target: Arc<AtomicIsize>,
    hotkeys: Vec<i32>,
    registered_hotkey: Hotkey,
    tray: Option<Tray>,
    hint_shown: bool,
}

/// The notification-area icon and its current NOTIFYICONDATAW payload.
struct Tray {
    data: NOTIFYICONDATAW,
    icon: HICON,
    owns_icon: bool,
}

impl WindowState {
    fn emit(&self, event: PlatformEvent) {
        let _ = self.events.send(event);
        (self.repaint)();
    }

    fn on_wake(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Hide => self.hide_target(),
                Command::Stop => {
                    unsafe {
                        PostQuitMessage(0);
                    }
                    return;
                }
            }
        }
    }

    fn on_tray(&mut self, lparam: LPARAM) {
        let notification = (lparam.0 as u32) & 0xFFFF;
        match notification {
            WM_LBUTTONDBLCLK => {
                self.show_target();
                self.emit(PlatformEvent::Show);
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => self.show_menu(),
            _ => {}
        }
    }

    fn show_menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("Show"));
            let _ = AppendMenuW(menu, MF_STRING, MENU_NEW_NOTE, w!("New note"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit"));

            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // Required so the menu dismisses when the user clicks elsewhere.
            let _ = SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);

            match command.0 as usize {
                MENU_SHOW => {
                    self.show_target();
                    self.emit(PlatformEvent::Show);
                }
                MENU_NEW_NOTE => {
                    self.show_target();
                    self.emit(PlatformEvent::NewNote);
                }
                MENU_QUIT => self.emit(PlatformEvent::Quit),
                _ => {}
            }
        }
    }

    fn show_target(&mut self) {
        let Some(hwnd) = self.resolve_target() else {
            return;
        };

        unsafe {
            let command = if IsIconic(hwnd).0 != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(hwnd, command);
            if SetForegroundWindow(hwnd).0 == 0 {
                force_foreground(hwnd);
            }
        }
    }

    fn resolve_target(&mut self) -> Option<HWND> {
        let stored = self.target.load(Ordering::SeqCst);
        if stored != 0 {
            let hwnd = HWND(stored as *mut c_void);
            if unsafe { IsWindow(Some(hwnd)).0 != 0 } {
                return Some(hwnd);
            }
            // The window was destroyed (or the app restarted): forget it.
            self.target.store(0, Ordering::SeqCst);
        }

        let hwnd = find_window_by_title(self.config.window_title.as_deref()?)?;
        self.target.store(hwnd.0 as isize, Ordering::SeqCst);
        Some(hwnd)
    }

    fn hide_target(&mut self) {
        if let Some(hwnd) = self.resolve_target() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        self.show_tray_hint();
    }

    fn show_tray_hint(&mut self) {
        if self.hint_shown {
            return;
        }
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        self.hint_shown = true;

        let mut data = tray.data;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        write_utf16(&mut data.szInfoTitle, "Rustboard");
        let message = format!(
            "Still running in the tray. Press {} to reopen, or right-click the tray icon to quit.",
            self.registered_hotkey
        );
        write_utf16(&mut data.szInfo, &message);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn cleanup(&mut self) {
        unsafe {
            for hotkey_id in self.hotkeys.drain(..) {
                let _ = UnregisterHotKey(Some(self.hwnd), hotkey_id);
            }
            if let Some(tray) = self.tray.take() {
                let _ = Shell_NotifyIconW(NIM_DELETE, &tray.data);
                if tray.owns_icon {
                    let _ = DestroyIcon(tray.icon);
                }
            }
            if !self.hwnd.is_invalid() {
                let hwnd = self.hwnd;
                self.hwnd = HWND::default();
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

/// Run the platform thread until WM_QUIT.
fn run_message_thread(args: ThreadArgs) {
    let ThreadArgs {
        config,
        repaint,
        events,
        commands,
        target,
        started,
        ready,
        done,
    } = args;

    let thread_id = unsafe { GetCurrentThreadId() };
    if started.send(thread_id).is_err() {
        return;
    }

    if let Err(error) = ensure_window_class() {
        let _ = ready.send(Err(error));
        return;
    }

    let preferred_hotkey = config.hotkey;
    let mut state = WindowState {
        hwnd: HWND::default(),
        config,
        repaint,
        events,
        commands,
        target,
        hotkeys: Vec::new(),
        registered_hotkey: preferred_hotkey,
        tray: None,
        hint_shown: false,
    };

    match unsafe { create_platform_window(&mut state) } {
        Ok(hwnd) => state.hwnd = hwnd,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    }

    if let Err(error) = register_hotkeys(&mut state) {
        state.cleanup();
        let _ = ready.send(Err(error));
        return;
    }

    if state.config.tray_icon
        && let Err(error) = add_tray(&mut state)
    {
        state.cleanup();
        let _ = ready.send(Err(error));
        return;
    }

    if ready.send(Ok(state.registered_hotkey)).is_err() {
        state.cleanup();
        return;
    }

    unsafe {
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0);
            if result.0 <= 0 {
                break;
            }
            // PostThreadMessageW messages have no window: DispatchMessageW would
            // drop them, so handle the command wake-up here.
            if message.hwnd.is_invalid() {
                if message.message == WM_APP_WAKE {
                    state.on_wake();
                }
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        state.cleanup();
    }

    let _ = done.send(());
}

/// Register the platform window class once per process.
fn ensure_window_class() -> Result<(), PlatformError> {
    static REGISTRATION: OnceLock<Result<(), u32>> = OnceLock::new();
    match *REGISTRATION.get_or_init(|| unsafe { register_window_class() }) {
        Ok(()) => Ok(()),
        Err(code) => Err(PlatformError::Win32 {
            op: "RegisterClassW",
            code,
        }),
    }
}

unsafe fn register_window_class() -> Result<(), u32> {
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).map_err(|_| last_error_code())?;
        let window_class = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(platform_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: HINSTANCE(module.0),
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_name(),
        };
        let atom = RegisterClassW(&window_class);
        if atom == 0 {
            Err(last_error_code())
        } else {
            Ok(())
        }
    }
}

unsafe fn create_platform_window(state: &mut WindowState) -> Result<HWND, PlatformError> {
    unsafe {
        let module =
            GetModuleHandleW(PCWSTR::null()).map_err(|_| win32_error("GetModuleHandleW"))?;
        let state_pointer = std::ptr::from_mut(state).cast::<c_void>();
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name(),
            window_name(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(HINSTANCE(module.0)),
            Some(state_pointer),
        )
        .map_err(|_| win32_error("CreateWindowExW"))
    }
}

unsafe extern "system" fn platform_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_DESTROY {
            // Do not touch WindowState here: cleanup calls DestroyWindow while
            // it holds a mutable borrow of the state.
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if message == WM_NCDESTROY {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }

        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
        if pointer.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let state = &mut *pointer;

        match message {
            WM_HOTKEY | WM_APP_SHOW => {
                state.show_target();
                state.emit(PlatformEvent::Show);
            }
            WM_APP_TRAY => state.on_tray(lparam),
            _ => return DefWindowProcW(hwnd, message, wparam, lparam),
        }
        LRESULT(0)
    }
}

/// Register one chord, returning the Win32 error code on failure.
fn try_register_hotkey(state: &WindowState, id: i32, hotkey: Hotkey) -> Result<(), u32> {
    unsafe {
        RegisterHotKey(
            Some(state.hwnd),
            id,
            win32_modifiers(hotkey),
            win32_virtual_key(hotkey.key),
        )
        .map_err(|_| last_error_code())
    }
}

fn register_hotkeys(state: &mut WindowState) -> Result<(), PlatformError> {
    let primary = state.config.hotkey;
    match try_register_hotkey(state, HOTKEY_PRIMARY_ID, primary) {
        Ok(()) => {
            state.hotkeys.push(HOTKEY_PRIMARY_ID);
            state.registered_hotkey = primary;
            Ok(())
        }
        Err(primary_code) => {
            let Some(fallback) = state.config.fallback_hotkey else {
                return Err(PlatformError::HotkeyUnavailable {
                    hotkey: primary,
                    code: primary_code,
                });
            };
            match try_register_hotkey(state, HOTKEY_FALLBACK_ID, fallback) {
                Ok(()) => {
                    state.hotkeys.push(HOTKEY_FALLBACK_ID);
                    state.registered_hotkey = fallback;
                    Ok(())
                }
                Err(fallback_code) => Err(PlatformError::BothHotkeysUnavailable {
                    primary,
                    primary_code,
                    fallback,
                    fallback_code,
                }),
            }
        }
    }
}

fn win32_modifiers(hotkey: Hotkey) -> HOT_KEY_MODIFIERS {
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    if hotkey.ctrl {
        modifiers |= MOD_CONTROL;
    }
    if hotkey.alt {
        modifiers |= MOD_ALT;
    }
    if hotkey.shift {
        modifiers |= MOD_SHIFT;
    }
    if hotkey.win {
        modifiers |= MOD_WIN;
    }
    // Holding the chord should not emit a stream of Show events.
    modifiers |= MOD_NOREPEAT;
    modifiers
}

fn win32_virtual_key(key: HotkeyKey) -> u32 {
    match key {
        HotkeyKey::Letter(letter) => u32::from(VK_A.0) + u32::from(letter - b'A'),
        HotkeyKey::Digit(digit) => u32::from(VK_0.0) + u32::from(digit),
        HotkeyKey::Function(number) => u32::from(VK_F1.0) + u32::from(number - 1),
        HotkeyKey::Space => u32::from(VK_SPACE.0),
        HotkeyKey::Up => u32::from(VK_UP.0),
        HotkeyKey::Down => u32::from(VK_DOWN.0),
        HotkeyKey::Left => u32::from(VK_LEFT.0),
        HotkeyKey::Right => u32::from(VK_RIGHT.0),
        HotkeyKey::Enter => u32::from(VK_RETURN.0),
        HotkeyKey::Escape => u32::from(VK_ESCAPE.0),
        HotkeyKey::Tab => u32::from(VK_TAB.0),
    }
}

fn add_tray(state: &mut WindowState) -> Result<(), PlatformError> {
    unsafe {
        let (icon, owns_icon) = load_tray_icon(state);
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: state.hwnd,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            ..Default::default()
        };
        if !icon.0.is_null() {
            data.uFlags |= NIF_ICON;
            data.hIcon = icon;
        }
        write_utf16(&mut data.szTip, &state.config.tooltip);

        if Shell_NotifyIconW(NIM_ADD, &data).0 == 0 {
            if owns_icon {
                let _ = DestroyIcon(icon);
            }
            return Err(win32_error("Shell_NotifyIconW(NIM_ADD)"));
        }

        state.tray = Some(Tray {
            data,
            icon,
            owns_icon,
        });
        Ok(())
    }
}

unsafe fn load_tray_icon(state: &WindowState) -> (HICON, bool) {
    unsafe {
        if let Some(path) = tray_icon_path(state)
            && let Some(icon) = load_icon_file(&path)
        {
            return (icon, true);
        }

        let stored = state.target.load(Ordering::SeqCst);
        let target = if stored != 0 {
            Some(HWND(stored as *mut c_void))
        } else {
            state
                .config
                .window_title
                .as_deref()
                .and_then(find_window_by_title)
        };
        if let Some(target) = target
            && let Some(icon) = window_icon(target)
        {
            return (icon, false);
        }

        match LoadIconW(None, IDI_APPLICATION) {
            Ok(icon) if !icon.0.is_null() => (icon, false),
            _ => (HICON::default(), false),
        }
    }
}

fn tray_icon_path(state: &WindowState) -> Option<PathBuf> {
    if let Some(path) = &state.config.icon_path {
        return Some(path.clone());
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("assets").join("rustboard.ico"));
        candidates.push(directory.join("rustboard.ico"));
    }
    // Development convenience: cargo run places the executable under target/,
    // so the repository asset is the only copy next to no executable.
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("rustboard.ico"),
    );
    candidates.into_iter().find(|path| path.is_file())
}

unsafe fn load_icon_file(path: &Path) -> Option<HICON> {
    unsafe {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let width = GetSystemMetrics(SM_CXSMICON);
        let height = GetSystemMetrics(SM_CYSMICON);
        LoadImageW(
            None,
            PCWSTR(wide.as_ptr()),
            IMAGE_ICON,
            width,
            height,
            LR_LOADFROMFILE,
        )
        .ok()
        .map(|handle| HICON(handle.0))
        .filter(|icon| !icon.0.is_null())
    }
}

unsafe fn window_icon(target: HWND) -> Option<HICON> {
    unsafe {
        for which in [ICON_SMALL, ICON_BIG] {
            let result = SendMessageW(
                target,
                WM_GETICON,
                Some(WPARAM(which as usize)),
                Some(LPARAM(0)),
            );
            if result.0 != 0 {
                return Some(HICON(result.0 as *mut c_void));
            }
        }
        let class_icon = GetClassLongPtrW(target, GCLP_HICONSM);
        if class_icon != 0 {
            return Some(HICON(class_icon as *mut c_void));
        }
        None
    }
}

fn write_utf16(buffer: &mut [u16], text: &str) {
    let limit = buffer.len().saturating_sub(1);
    let mut written = 0;
    for unit in text.encode_utf16().take(limit) {
        buffer[written] = unit;
        written += 1;
    }
    if written < buffer.len() {
        buffer[written] = 0;
    }
}

unsafe fn force_foreground(hwnd: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(foreground_thread, current_thread, true).0 != 0;

        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        if attached {
            let _ = AttachThreadInput(foreground_thread, current_thread, false);
        }
    }
}

fn find_window_by_title(title: &str) -> Option<HWND> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }
        .ok()
        .filter(|hwnd| !hwnd.is_invalid())
}

fn find_platform_window() -> Option<HWND> {
    unsafe {
        if let Ok(hwnd) = FindWindowW(class_name(), PCWSTR::null())
            && !hwnd.is_invalid()
        {
            return Some(hwnd);
        }
        if let Ok(hwnd) = FindWindowExW(Some(HWND_MESSAGE), None, class_name(), PCWSTR::null())
            && !hwnd.is_invalid()
        {
            return Some(hwnd);
        }
        None
    }
}

fn forward_show_to_existing() {
    for _ in 0..FORWARD_ATTEMPTS {
        if let Some(hwnd) = find_platform_window() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_APP_SHOW, WPARAM(0), LPARAM(0));
            }
            return;
        }
        thread::sleep(FORWARD_POLL);
    }
}

fn class_name() -> PCWSTR {
    w!("Rustboard.PlatformWindow")
}

fn window_name() -> PCWSTR {
    w!("Rustboard Platform")
}

fn mutex_name() -> PCWSTR {
    w!("Local\\Rustboard.SingleInstance")
}

fn last_error_code() -> u32 {
    unsafe { GetLastError().0 }
}

fn win32_error(op: &'static str) -> PlatformError {
    PlatformError::Win32 {
        op,
        code: last_error_code(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{Platform, PlatformHandle};
    use crate::platform::{Hotkey, PlatformConfig, PlatformError, PlatformEvent, PlatformStart};

    /// The platform is a process-wide singleton, so its tests must not overlap.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn test_config() -> PlatformConfig {
        PlatformConfig {
            hotkey: Hotkey::parse("Ctrl+Alt+Shift+F13").expect("valid hotkey"),
            fallback_hotkey: Some(Hotkey::parse("Ctrl+Alt+Shift+F14").expect("valid hotkey")),
            tray_icon: false,
            window_title: Some(String::from("Rustboard platform test target")),
            ..PlatformConfig::default()
        }
    }

    fn start_config(
        config: PlatformConfig,
        repaints: &Arc<AtomicUsize>,
    ) -> Result<PlatformStart, PlatformError> {
        let hook = {
            let repaints = Arc::clone(repaints);
            move || {
                repaints.fetch_add(1, Ordering::SeqCst);
            }
        };
        Platform::start(config, hook)
    }

    fn start_platform(repaints: &Arc<AtomicUsize>) -> Result<PlatformStart, PlatformError> {
        start_config(test_config(), repaints)
    }

    /// Start a handle, or return None when the environment cannot support one
    /// (no interactive window station, hotkey already owned, another Rustboard
    /// running). The skip is reported so a CI log explains it.
    fn started_or_skip(repaints: &Arc<AtomicUsize>) -> Option<PlatformHandle> {
        match start_platform(repaints) {
            Ok(PlatformStart::Started(handle)) => Some(handle),
            Ok(PlatformStart::ForwardedToExisting) => {
                eprintln!("skipping platform test: another Rustboard instance is running");
                None
            }
            Err(error) => {
                eprintln!("skipping platform test: {error}");
                None
            }
        }
    }

    fn wait_for_event(handle: &PlatformHandle, timeout: Duration) -> Option<PlatformEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = handle.take_event() {
                return Some(event);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn start_stop_and_restart_leave_no_resources() {
        let _guard = lock_tests();
        let repaints = Arc::new(AtomicUsize::new(0));
        let Some(mut handle) = started_or_skip(&repaints) else {
            return;
        };

        assert_eq!(handle.take_event(), None);
        assert!(
            matches!(
                handle.hotkey().to_string().as_str(),
                "Ctrl+Alt+Shift+F13" | "Ctrl+Alt+Shift+F14"
            ),
            "the preferred chord or its fallback must be registered"
        );
        handle.request_hide().expect("hide command");
        handle.request_hide().expect("hide command is repeatable");
        handle.set_target_window(0);
        handle.stop().expect("stop");
        handle.stop().expect("stop is idempotent");
        assert!(
            super::find_platform_window().is_none(),
            "the platform window must be destroyed by stop()"
        );
        drop(handle);
        assert!(
            super::find_platform_window().is_none(),
            "the platform window must be destroyed by Drop"
        );

        let mut restarted = match start_platform(&repaints) {
            Ok(PlatformStart::Started(handle)) => handle,
            Ok(PlatformStart::ForwardedToExisting) => {
                panic!("restart was forwarded after the first instance stopped")
            }
            Err(error) => panic!("hotkey or single-instance mutex leaked after stop: {error}"),
        };
        assert_eq!(restarted.take_event(), None);
        restarted.stop().expect("second stop");
    }

    #[test]
    fn drop_releases_the_hotkey_and_single_instance() {
        let _guard = lock_tests();
        let repaints = Arc::new(AtomicUsize::new(0));
        let Some(handle) = started_or_skip(&repaints) else {
            return;
        };

        drop(handle);
        assert!(
            super::find_platform_window().is_none(),
            "Drop must destroy the platform window"
        );

        let mut restarted = match start_platform(&repaints) {
            Ok(PlatformStart::Started(handle)) => handle,
            Ok(PlatformStart::ForwardedToExisting) => {
                panic!("restart was forwarded after the handle was dropped")
            }
            Err(error) => panic!("resources leaked after Drop: {error}"),
        };
        restarted.stop().expect("stop");
    }

    #[test]
    fn second_start_forwards_show_and_requests_repaint() {
        let _guard = lock_tests();
        let repaints = Arc::new(AtomicUsize::new(0));
        let Some(mut first) = started_or_skip(&repaints) else {
            return;
        };

        match start_platform(&repaints) {
            Ok(PlatformStart::ForwardedToExisting) => {}
            Ok(PlatformStart::Started(handle)) => {
                drop(handle);
                panic!("a second platform started instead of forwarding");
            }
            Err(error) => panic!("second start failed instead of forwarding: {error}"),
        }

        let event = wait_for_event(&first, Duration::from_secs(2));
        assert_eq!(
            event,
            Some(PlatformEvent::Show),
            "a second launch should post Show to the first instance"
        );
        assert!(
            repaints.load(Ordering::SeqCst) > 0,
            "the repaint hook must run so a hidden app wakes up"
        );

        // Drain anything else, then confirm the queue is empty.
        while first.take_event().is_some() {}
        assert_eq!(first.take_event(), None, "take_event drains the queue");
        first.stop().expect("stop");
    }

    #[test]
    fn tray_icon_can_be_added_and_removed() {
        let _guard = lock_tests();
        let repaints = Arc::new(AtomicUsize::new(0));
        let mut config = test_config();
        config.tray_icon = true;

        match start_config(config, &repaints) {
            Ok(PlatformStart::Started(mut handle)) => {
                handle.stop().expect("stop with a tray icon");
                assert!(
                    super::find_platform_window().is_none(),
                    "the tray window must be destroyed by stop()"
                );
            }
            Ok(PlatformStart::ForwardedToExisting) => {
                eprintln!("skipping tray test: another Rustboard instance is running");
            }
            // A shell-less session (no Explorer notification area) cannot own a
            // tray icon. Any other failure is a real bug and must fail.
            Err(PlatformError::Win32 {
                op: "Shell_NotifyIconW(NIM_ADD)",
                ..
            }) => {
                eprintln!("skipping tray test: no notification area in this session");
            }
            Err(error) => panic!("tray start failed: {error}"),
        }
    }
}
