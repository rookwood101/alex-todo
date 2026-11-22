//! Windows platform integration for Rustboard (task 160).
//!
//! This module owns the native pieces that make Rustboard a background app:
//!
//! - a dedicated Win32 message thread with a message-only window
//!   ([`Platform::start`]);
//! - a global hotkey (two-key chord, with a fallback chord if the first one is
//!   already owned by another program);
//! - a notification-area (tray) icon with a Show / New note / Quit menu and
//!   double-click-to-show;
//! - hide-to-tray plumbing ([`PlatformHandle::request_hide`]), including a
//!   one-time "still running in the tray" balloon;
//! - single-instance forwarding: a second launch finds the running platform
//!   window, posts a Show to it, and exits 0.
//!
//! The platform module never owns the eframe window. It is told which HWND to
//! show and focus, either explicitly with
//! [`PlatformHandle::set_target_window`] or implicitly through
//! [`PlatformConfig::window_title`] (the first window with that title is used
//! when no explicit handle has been set).
//!
//! # Wiring for task 170
//!
//! Start the platform once, before `eframe::run_native`, so a second launch can
//! exit before it ever creates a window. The repaint hook needs the egui
//! context, which only exists inside `eframe`; a `OnceLock` bridges the two:
//!
//! ```ignore
//! use std::sync::OnceLock;
//! use eframe::egui;
//! use crate::platform::{Platform, PlatformConfig, PlatformEvent, PlatformHandle, PlatformStart};
//!
//! static UI_CONTEXT: OnceLock<egui::Context> = OnceLock::new();
//!
//! fn main() -> eframe::Result {
//!     let platform = match Platform::start(PlatformConfig::default(), || {
//!         if let Some(ctx) = UI_CONTEXT.get() {
//!             ctx.request_repaint();
//!         }
//!     }) {
//!         Ok(PlatformStart::Started(handle)) => Some(handle),
//!         // Another Rustboard owns the hotkey/tray and has just been shown.
//!         Ok(PlatformStart::ForwardedToExisting) => return Ok(()),
//!         Err(error) => {
//!             eprintln!("platform init failed: {error}");
//!             None
//!         }
//!     };
//!
//!     eframe::run_native(
//!         "Rustboard",
//!         options,
//!         Box::new(move |cc| {
//!             let _ = UI_CONTEXT.set(cc.egui_ctx.clone());
//!             let mut app = RustboardApp::new(cc);
//!             app.platform = platform;
//!             Ok(Box::new(app))
//!         }),
//!     )
//! }
//! ```
//!
//! Then, in the app:
//!
//! 1. Drain events from `App::logic` (which eframe also calls while the window
//!    is hidden, but only after the platform's repaint hook fires):
//!
//!    ```ignore
//!    while let Some(event) = self.platform.as_ref().and_then(PlatformHandle::take_event) {
//!        match event {
//!            PlatformEvent::Show => {
//!                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
//!                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
//!                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
//!            }
//!            PlatformEvent::NewNote => { /* show, then AppAction::AddNote */ }
//!            PlatformEvent::Quit => { /* set a quitting flag, then ViewportCommand::Close */ }
//!            PlatformEvent::Toggle => {}
//!        }
//!    }
//!    ```
//!
//! 2. Intercept X. In `logic` (and/or `ui`) check
//!    `ctx.input(|i| i.viewport().close_requested())`. Unless the app is
//!    quitting, send `ViewportCommand::CancelClose` and
//!    `ViewportCommand::Visible(false)`, then call
//!    `platform.request_hide()` so the native window is hidden immediately and
//!    the one-time tray balloon is shown. On the Quit event, do not cancel the
//!    close, or the process will never exit.
//!
//! 3. Esc hides only when no editor wants the keyboard: check
//!    `ctx.input(|i| i.key_pressed(egui::Key::Escape))` and
//!    `!ctx.egui_wants_keyboard_input()`, then do the same as X (without
//!    `CancelClose`).
//!
//! 4. In `App::on_exit`, call `platform.stop()` (the `Drop` impl also stops the
//!    thread and removes the tray icon; the explicit call just surfaces errors).
//!
//! 5. `PlatformConfig::window_title` defaults to `"Rustboard"`, so the Show
//!    path can find the eframe window without a handle. If 170 can supply a raw
//!    HWND (for example `frame.winit_window()` plus the `raw-window-handle`
//!    crate's `HasWindowHandle`), call
//!    `platform.set_target_window(hwnd)` once; that is more precise when other
//!    windows share the title.
//!
//! # Behaviour notes
//!
//! - Only one chord is registered: the configured `hotkey`, or
//!   `fallback_hotkey` if the first call to `RegisterHotKey` fails (for example
//!   with Win32 error 1409, "hotkey already registered"). If both fail,
//!   [`Platform::start`] returns a typed [`PlatformError`].
//! - The tray icon and hotkey are removed when the handle is stopped or
//!   dropped. The platform thread exits its message loop and is joined; tests
//!   never block indefinitely because shutdown uses a timeout.
//! - The tray icon needs `assets/rustboard.ico` next to the executable (or
//!   `PlatformConfig::icon_path`). During `cargo run` from the repository the
//!   manifest copy is used as a development fallback; otherwise the target
//!   window's icon, then the stock application icon, is used.
//! - `#[cfg(windows)]` only: `win32.rs` carries the implementation. Other
//!   targets get a stub that returns [`PlatformError::Unsupported`] so the crate
//!   still compiles.

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

/// A parsed global-hotkey chord.
///
/// Build chords with [`Hotkey::parse`] (`"Ctrl+Space"`, `"Ctrl+Alt+Shift+F13"`)
/// and render them with [`Display`](fmt::Display). At least one modifier is
/// required; exactly one non-modifier key is allowed.
///
/// The default is Ctrl+Space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Hotkey {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: HotkeyKey,
}

/// The non-modifier part of a [`Hotkey`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum HotkeyKey {
    /// `A`..`Z`, stored as the uppercase ASCII byte.
    Letter(u8),
    /// `0`..`9`, stored as the digit value.
    Digit(u8),
    /// `F1`..`F24`.
    Function(u8),
    Space,
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Tab,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            win: false,
            key: HotkeyKey::Space,
        }
    }
}

impl Hotkey {
    /// Parse a chord such as `"Ctrl+Space"` or `"Ctrl+Alt+Shift+F13"`.
    ///
    /// Modifiers are matched case-insensitively and accept the synonyms
    /// `Ctrl`/`Control`, `Win`/`Windows`/`Super`/`Meta`, `Esc`/`Escape` and
    /// `Return`/`Enter`. Keys are `A`-`Z`, `0`-`9`, `F1`-`F24`, `Space`,
    /// `Up`, `Down`, `Left`, `Right`, `Enter`, `Escape` and `Tab`.
    ///
    /// # Errors
    ///
    /// Returns a [`HotkeyError`] if the chord is empty, uses an unknown token,
    /// repeats a modifier, contains more than one key, has no modifier, or has
    /// no key.
    pub fn parse(input: &str) -> Result<Self, HotkeyError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(HotkeyError::Empty);
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut win = false;
        let mut key = None;

        for part in trimmed.split('+') {
            let token = part.trim();
            if token.is_empty() {
                return Err(HotkeyError::EmptyPart);
            }

            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    if ctrl {
                        return Err(HotkeyError::DuplicateModifier { name: "Ctrl" });
                    }
                    ctrl = true;
                }
                "alt" => {
                    if alt {
                        return Err(HotkeyError::DuplicateModifier { name: "Alt" });
                    }
                    alt = true;
                }
                "shift" => {
                    if shift {
                        return Err(HotkeyError::DuplicateModifier { name: "Shift" });
                    }
                    shift = true;
                }
                "win" | "windows" | "super" | "meta" => {
                    if win {
                        return Err(HotkeyError::DuplicateModifier { name: "Win" });
                    }
                    win = true;
                }
                other => {
                    if key.is_some() {
                        return Err(HotkeyError::MultipleKeys);
                    }
                    key = Some(parse_key(other).ok_or_else(|| HotkeyError::UnknownToken {
                        token: token.to_owned(),
                    })?);
                }
            }
        }

        if !(ctrl || alt || shift || win) {
            return Err(HotkeyError::MissingModifier);
        }
        let key = key.ok_or(HotkeyError::MissingKey)?;

        Ok(Self {
            ctrl,
            alt,
            shift,
            win,
            key,
        })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (enabled, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.win, "Win"),
        ] {
            if enabled {
                if !first {
                    formatter.write_str("+")?;
                }
                formatter.write_str(name)?;
                first = false;
            }
        }
        if !first {
            formatter.write_str("+")?;
        }
        fmt_key(self.key, formatter)
    }
}

fn parse_key(token: &str) -> Option<HotkeyKey> {
    if token.len() == 1 {
        let byte = token.as_bytes()[0];
        return match byte {
            b'a'..=b'z' => Some(HotkeyKey::Letter(byte.to_ascii_uppercase())),
            b'0'..=b'9' => Some(HotkeyKey::Digit(byte - b'0')),
            _ => None,
        };
    }

    if let Some(digits) = token.strip_prefix('f') {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let number: u8 = digits.parse().ok()?;
        return (1..=24)
            .contains(&number)
            .then_some(HotkeyKey::Function(number));
    }

    match token {
        "space" | "spacebar" => Some(HotkeyKey::Space),
        "up" | "arrowup" => Some(HotkeyKey::Up),
        "down" | "arrowdown" => Some(HotkeyKey::Down),
        "left" | "arrowleft" => Some(HotkeyKey::Left),
        "right" | "arrowright" => Some(HotkeyKey::Right),
        "enter" | "return" => Some(HotkeyKey::Enter),
        "escape" | "esc" => Some(HotkeyKey::Escape),
        "tab" => Some(HotkeyKey::Tab),
        _ => None,
    }
}

fn fmt_key(key: HotkeyKey, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match key {
        HotkeyKey::Letter(letter) => write!(formatter, "{}", char::from(letter)),
        HotkeyKey::Digit(digit) => write!(formatter, "{digit}"),
        HotkeyKey::Function(number) => write!(formatter, "F{number}"),
        HotkeyKey::Space => formatter.write_str("Space"),
        HotkeyKey::Up => formatter.write_str("Up"),
        HotkeyKey::Down => formatter.write_str("Down"),
        HotkeyKey::Left => formatter.write_str("Left"),
        HotkeyKey::Right => formatter.write_str("Right"),
        HotkeyKey::Enter => formatter.write_str("Enter"),
        HotkeyKey::Escape => formatter.write_str("Escape"),
        HotkeyKey::Tab => formatter.write_str("Tab"),
    }
}

/// Errors from [`Hotkey::parse`].
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HotkeyError {
    /// The input was empty or only whitespace.
    #[error("a hotkey cannot be empty")]
    Empty,
    /// The input contained an empty `+` segment, such as `"Ctrl++Space"`.
    #[error("a hotkey cannot contain an empty + segment")]
    EmptyPart,
    /// A token was neither a modifier nor a supported key name.
    #[error("{token} is not a supported hotkey key or modifier")]
    UnknownToken {
        /// The offending token, as written by the user.
        token: String,
    },
    /// The same modifier appeared twice.
    #[error("modifier {name} appears more than once")]
    DuplicateModifier {
        /// Canonical modifier name.
        name: &'static str,
    },
    /// The chord had no Ctrl, Alt, Shift or Win modifier.
    #[error("a hotkey needs at least one of Ctrl, Alt, Shift or Win")]
    MissingModifier,
    /// The chord had only modifiers.
    #[error("a hotkey needs exactly one non-modifier key")]
    MissingKey,
    /// The chord had more than one non-modifier key.
    #[error("a hotkey can only contain one non-modifier key")]
    MultipleKeys,
}

/// Configuration for [`Platform::start`].
///
/// `PlatformConfig::default()` gives Ctrl+Space with a Ctrl+Alt+Space fallback,
/// tooltip `"Rustboard"`, the icon next to the executable, and the eframe window
/// title `"Rustboard"`.
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    /// The preferred global-hotkey chord.
    pub hotkey: Hotkey,
    /// A chord to try when `hotkey` cannot be registered (for example because
    /// another program already owns it). `None` disables the fallback.
    pub fallback_hotkey: Option<Hotkey>,
    /// Tray-icon tooltip. Keep it short (128 UTF-16 units at most).
    pub tooltip: String,
    /// Icon to load from disk. `None` looks for `assets/rustboard.ico` (then
    /// `rustboard.ico`) next to the executable, then the window icon, then the
    /// stock application icon.
    pub icon_path: Option<PathBuf>,
    /// Window title used to locate the eframe window when no explicit handle
    /// has been set with [`PlatformHandle::set_target_window`]. `None` disables
    /// title-based lookup.
    pub window_title: Option<String>,
    /// Add the notification-area icon. Tests and headless runs can disable this
    /// to avoid depending on the shell.
    pub tray_icon: bool,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            hotkey: Hotkey::default(),
            fallback_hotkey: Some(Hotkey {
                ctrl: true,
                alt: true,
                shift: false,
                win: false,
                key: HotkeyKey::Space,
            }),
            tooltip: String::from("Rustboard"),
            icon_path: None,
            window_title: Some(String::from("Rustboard")),
            tray_icon: true,
        }
    }
}

/// An event produced by the platform thread.
///
/// The app drains these from [`PlatformHandle::take_event`] each frame. Every
/// event is also followed by a repaint request through the hook passed to
/// [`Platform::start`], so the app runs even while the window is hidden.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformEvent {
    /// Surface the window: hotkey, tray double-click, tray "Show", or a second
    /// launch forwarding to this instance.
    Show,
    /// The tray "New note" item was chosen. The window should also be shown.
    NewNote,
    /// The tray "Quit" item was chosen. The app must exit, not hide.
    Quit,
    /// Reserved for callers that bind a toggle gesture; the built-in gestures
    /// emit [`PlatformEvent::Show`] instead.
    Toggle,
}

/// Errors from starting or controlling the platform.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PlatformError {
    /// The primary chord was rejected and no fallback was configured.
    #[error("hotkey {hotkey} could not be registered (Win32 error {code})")]
    HotkeyUnavailable {
        /// The rejected chord.
        hotkey: Hotkey,
        /// The Win32 error code, normally 1409 ("already registered").
        code: u32,
    },
    /// Both the configured chord and its fallback were rejected.
    #[error(
        "hotkeys {primary} (Win32 error {primary_code}) and {fallback} (Win32 error {fallback_code}) could not be registered"
    )]
    BothHotkeysUnavailable {
        /// The configured chord.
        primary: Hotkey,
        /// Win32 error code for the configured chord.
        primary_code: u32,
        /// The fallback chord.
        fallback: Hotkey,
        /// Win32 error code for the fallback chord.
        fallback_code: u32,
    },
    /// A Win32 call failed.
    #[error("Win32 call {op} failed (error {code})")]
    Win32 {
        /// The call that failed.
        op: &'static str,
        /// The value of `GetLastError` after the call.
        code: u32,
    },
    /// The platform thread could not be spawned.
    #[error("could not spawn the platform thread: {0}")]
    ThreadSpawn(String),
    /// The platform thread did not become ready within the start timeout.
    #[error("the platform thread did not start in time")]
    StartTimeout,
    /// The platform thread stopped before it became ready.
    #[error("the platform thread stopped unexpectedly")]
    ThreadStopped,
    /// The platform thread did not stop within the shutdown timeout.
    #[error("the platform thread did not stop in time")]
    ShutdownTimeout,
    /// The platform handle has already been stopped.
    #[error("the platform is not running")]
    Stopped,
    /// The platform is not implemented for this target.
    #[error("the platform is only supported on Windows")]
    Unsupported,
}

/// The outcome of [`Platform::start`].
pub enum PlatformStart {
    /// This process owns the platform thread; keep the handle for its lifetime.
    Started(PlatformHandle),
    /// Another Rustboard instance owns the hotkey and tray. A Show was posted
    /// to it and this process should exit with code 0.
    ///
    /// If the first instance's window cannot be found within about two seconds
    /// (for example because it is still starting), this process still exits
    /// rather than creating a duplicate instance.
    ForwardedToExisting,
}

#[cfg(windows)]
mod win32;
#[cfg(windows)]
#[allow(unused_imports)] // re-exported for task 170 to wire into the app
pub use win32::{Platform, PlatformHandle};

#[cfg(not(windows))]
mod unsupported {
    //! Stub for non-Windows targets so the crate still compiles.
    //!
    //! Rustboard's platform integration is Windows-only (task 160); starting it
    //! elsewhere reports [`PlatformError::Unsupported`].

    use super::{Hotkey, PlatformConfig, PlatformError, PlatformEvent, PlatformStart};

    /// Windows-only platform entry point.
    pub struct Platform;

    impl Platform {
        /// Always fails with [`PlatformError::Unsupported`].
        ///
        /// # Errors
        ///
        /// Always returns an error on this target.
        pub fn start(
            _config: PlatformConfig,
            _repaint: impl Fn() + Send + 'static,
        ) -> Result<PlatformStart, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    /// A handle that cannot be created on this target.
    pub struct PlatformHandle;

    impl PlatformHandle {
        /// Always returns None.
        pub fn take_event(&self) -> Option<PlatformEvent> {
            None
        }

        /// Always fails with [`PlatformError::Unsupported`].
        ///
        /// # Errors
        ///
        /// Always returns an error on this target.
        pub fn request_hide(&self) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported)
        }

        /// No-op on this target.
        pub fn set_target_window(&self, _hwnd: isize) {}

        /// Returns the default chord.
        pub fn hotkey(&self) -> Hotkey {
            Hotkey::default()
        }

        /// No-op on this target.
        ///
        /// # Errors
        ///
        /// Never fails.
        pub fn stop(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
#[allow(unused_imports)] // re-exported for task 170 to wire into the app
pub use unsupported::{Platform, PlatformHandle};

#[cfg(test)]
mod tests {
    use super::{Hotkey, HotkeyError};

    #[test]
    fn parses_the_documented_chords() {
        let cases = [
            ("Ctrl+Space", "Ctrl+Space"),
            ("ctrl+space", "Ctrl+Space"),
            ("Control + Space", "Ctrl+Space"),
            ("Ctrl+Alt+Space", "Ctrl+Alt+Space"),
            ("Ctrl+Alt+Shift+Win+F24", "Ctrl+Alt+Shift+Win+F24"),
            ("Win+A", "Win+A"),
            ("Shift+0", "Shift+0"),
            ("Alt+Up", "Alt+Up"),
            ("Alt+Down", "Alt+Down"),
            ("Alt+Left", "Alt+Left"),
            ("Alt+Right", "Alt+Right"),
            ("Ctrl+Enter", "Ctrl+Enter"),
            ("Ctrl+Return", "Ctrl+Enter"),
            ("Ctrl+Escape", "Ctrl+Escape"),
            ("Ctrl+Esc", "Ctrl+Escape"),
            ("Ctrl+Tab", "Ctrl+Tab"),
            ("Ctrl+F1", "Ctrl+F1"),
            ("Ctrl+F12", "Ctrl+F12"),
            ("Ctrl+ArrowUp", "Ctrl+Up"),
            ("Ctrl+Spacebar", "Ctrl+Space"),
        ];

        for (input, expected) in cases {
            let hotkey = Hotkey::parse(input).expect("documented chord parses");
            assert_eq!(hotkey.to_string(), expected, "input {input}");
        }
    }

    #[test]
    fn rejects_invalid_chords() {
        assert_eq!(Hotkey::parse(""), Err(HotkeyError::Empty));
        assert_eq!(Hotkey::parse("   "), Err(HotkeyError::Empty));
        assert_eq!(Hotkey::parse("Ctrl++Space"), Err(HotkeyError::EmptyPart));
        assert_eq!(Hotkey::parse("Ctrl+Space+"), Err(HotkeyError::EmptyPart));
        assert_eq!(
            Hotkey::parse("Ctrl+Nope"),
            Err(HotkeyError::UnknownToken {
                token: String::from("Nope")
            })
        );
        assert_eq!(
            Hotkey::parse("Ctrl+Ctrl+Space"),
            Err(HotkeyError::DuplicateModifier { name: "Ctrl" })
        );
        assert_eq!(
            Hotkey::parse("Ctrl+Control+Space"),
            Err(HotkeyError::DuplicateModifier { name: "Ctrl" })
        );
        assert_eq!(
            Hotkey::parse("Windows+Win+A"),
            Err(HotkeyError::DuplicateModifier { name: "Win" })
        );
        assert_eq!(Hotkey::parse("Space"), Err(HotkeyError::MissingModifier));
        assert_eq!(Hotkey::parse("Ctrl"), Err(HotkeyError::MissingKey));
        assert_eq!(
            Hotkey::parse("Ctrl+Space+A"),
            Err(HotkeyError::MultipleKeys)
        );
        assert_eq!(
            Hotkey::parse("Ctrl+F0"),
            Err(HotkeyError::UnknownToken {
                token: String::from("F0")
            })
        );
        assert_eq!(
            Hotkey::parse("Ctrl+F25"),
            Err(HotkeyError::UnknownToken {
                token: String::from("F25")
            })
        );
        assert_eq!(
            Hotkey::parse("Ctrl+1A"),
            Err(HotkeyError::UnknownToken {
                token: String::from("1A")
            })
        );
    }

    #[test]
    fn display_round_trips_through_parse() {
        for input in [
            "Ctrl+Space",
            "Ctrl+Alt+Space",
            "Ctrl+Alt+Shift+Win+F24",
            "Win+A",
            "Shift+0",
            "Alt+Up",
            "Alt+Down",
            "Alt+Left",
            "Alt+Right",
            "Ctrl+Enter",
            "Ctrl+Escape",
            "Ctrl+Tab",
            "Ctrl+F1",
            "Ctrl+F24",
        ] {
            let hotkey = Hotkey::parse(input).expect("valid chord");
            let rendered = hotkey.to_string();
            let reparsed = Hotkey::parse(&rendered).expect("rendered chord parses");
            assert_eq!(hotkey, reparsed, "round trip for {input}");
            assert_eq!(rendered, hotkey.to_string(), "stable rendering");
        }
    }

    #[test]
    fn default_is_ctrl_space() {
        assert_eq!(Hotkey::default().to_string(), "Ctrl+Space");
    }

    #[test]
    fn modifiers_are_rendered_in_canonical_order() {
        let hotkey = Hotkey::parse("Win+Shift+Alt+Ctrl+A").expect("valid chord");
        assert_eq!(hotkey.to_string(), "Ctrl+Alt+Shift+Win+A");
    }
}
