//! Windows toast notifications: AUMID registration, WinRT delivery and fallback.
//!
//! Windows only shows a toast when the sending process has an AppUserModelID
//! (AUMID) that the shell knows about. For an unpackaged desktop app that means
//! two pieces of per-user state:
//!
//! * a Start Menu shortcut named Rustboard.lnk whose System.AppUserModel.ID
//!   property is [AUMID], and
//! * HKCU\Software\Classes\AppUserModelId\com.rustboard.app with a DisplayName
//!   of [APP_DISPLAY_NAME].
//!
//! [ToastNotifier] creates both on first use, caches success for the process,
//! and shows toasts through [WinRtBackend]
//! (tauri-winrt-notification 0.8). A process-wide
//! SetCurrentProcessExplicitAppUserModelID call lives in [ensure_app_id], which
//! 170 calls once at startup.
//!
//! # Click-to-focus
//!
//! A toast click activates its shortcut, which starts a new process unless the
//! app has a single-instance forwarder and/or a toast activator CLSID. That is
//! task 170 work (single instance and the activator); this module deliberately
//! does not fake it. V1 has no toast action buttons.
//!
//! # Delivery policy
//!
//! If registration or show fails, the error is logged and the fallback is
//! invoked. When the fallback succeeds [Notifier::notify] returns Ok: the
//! reminder was delivered by the fallback and must not be retried or lost.
//! When the fallback also fails, notify returns [NotifyError::FallbackFailed]
//! so the caller can surface the loss. 170 installs the real fallback (taskbar
//! flash + in-app banner); until then the default [DefaultFallback] returns an
//! error because it cannot show anything, so notify reports the loss.
//!
//! # Threading
//!
//! notify is synchronous: it spawns and joins no threads. The WinRT call itself
//! contains a short library sleep, so 140 should deliver reminders from its
//! reminder thread rather than the egui frame.
//!
//! # Manual verification
//!
//! [test_notification] and [Notifier::test] send a real toast. They are for
//! manual checks only and are never called from unit tests.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri_winrt_notification::{Duration, Sound, Toast};
use windows::Win32::Foundation::{ERROR_SUCCESS, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemAlloc, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SetCurrentProcessExplicitAppUserModelID, ShellLink};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

/// The AppUserModelID shared by the shortcut, the registry key and the toast.
pub const AUMID: &str = "com.rustboard.app";

/// The name Windows shows in notification settings and toast headers.
pub const APP_DISPLAY_NAME: &str = "Rustboard";

/// File name of the Start Menu shortcut that registers [AUMID].
pub const SHORTCUT_FILE_NAME: &str = "Rustboard.lnk";

/// Icon file shipped in assets/ and used for the Start Menu shortcut.
pub const ICON_FILE_NAME: &str = "rustboard.ico";

/// One notification, as plain data with no UI types.
///
/// tag is carried for the caller (140 uses it to correlate a reminder with its
/// banner); tauri-winrt-notification 0.8 does not expose the WinRT toast tag,
/// so the tag is not forwarded to the toast itself. urgent selects a
/// long-lived toast with the reminder sound instead of a short default one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// Toast heading.
    pub title: String,
    /// Toast body text.
    pub body: String,
    /// Caller-owned correlation tag; also included in failure logs.
    pub tag: String,
    /// Use a long duration and the reminder sound.
    pub urgent: bool,
}

impl Notification {
    /// A normal-priority notification with an empty tag.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            tag: String::new(),
            urgent: false,
        }
    }

    /// Attach a caller-owned correlation tag.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    /// Mark the notification as urgent (long duration, reminder sound).
    pub fn urgent(mut self) -> Self {
        self.urgent = true;
        self
    }
}

/// Errors from registration, toast delivery or the fallback.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// %APPDATA% is missing or empty.
    #[error("cannot determine the user's AppData folder: set APPDATA")]
    MissingAppData,

    /// The running executable could not be resolved for the shortcut target.
    #[error("cannot resolve the running executable: {0}")]
    CurrentExe(#[source] io::Error),

    /// A parent directory needed for the shortcut could not be created.
    #[error("cannot create directory {path}: {source}")]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The shortcut path has no usable parent directory.
    #[error("shortcut path {path} has no parent directory")]
    InvalidShortcutPath {
        /// Rejected shortcut path.
        path: PathBuf,
    },

    /// Per-thread COM initialization failed.
    #[error("cannot initialize COM: {0}")]
    ComInit(#[source] windows::core::Error),

    /// A Windows shell/COM call failed.
    #[error("Windows shell error: {0}")]
    Shell(#[from] windows::core::Error),

    /// A specific shortcut-registration step failed.
    #[error("Windows shell error during {step}: {source}")]
    ShellStep {
        /// Human-readable step name.
        step: &'static str,
        /// Underlying Windows error.
        #[source]
        source: windows::core::Error,
    },

    /// SetCurrentProcessExplicitAppUserModelID failed.
    #[error("cannot set the process AppUserModelID: {0}")]
    ProcessAppId(#[source] windows::core::Error),

    /// The HKCU AppUserModelId registration key could not be written.
    #[error("cannot write the AppUserModelId registry key: {0}")]
    Registry(#[source] windows::core::Error),

    /// Converting the AUMID to a native property value ran out of memory.
    #[error("out of memory while preparing the notification property")]
    OutOfMemory,

    /// An injected toast backend failed.
    #[error("notification backend failed: {message}")]
    Backend {
        /// Backend-specific message.
        message: String,
    },

    /// An injected registration backend failed.
    #[error("registration backend failed: {message}")]
    Registration {
        /// Backend-specific message.
        message: String,
    },

    /// The real toast was rejected by Windows.
    #[error("toast failed: {0}")]
    Show(#[source] tauri_winrt_notification::Error),

    /// An injected fallback failed.
    #[error("notification fallback failed: {message}")]
    Fallback {
        /// Fallback-specific message.
        message: String,
    },

    /// Both the toast and its fallback failed, so the reminder was not delivered.
    #[error("toast and fallback both failed (toast: {primary}; fallback: {fallback})")]
    FallbackFailed {
        /// The original toast or registration error.
        #[source]
        primary: Box<NotifyError>,
        /// The fallback error.
        fallback: Box<NotifyError>,
    },
}

/// Anything that can deliver a [Notification].
pub trait Notifier: Send + Sync {
    /// Deliver one notification.
    ///
    /// Returns Ok once the toast or the fallback accepted the notification; see
    /// the module documentation for the exact failure policy.
    fn notify(&self, notification: Notification) -> Result<(), NotifyError>;

    /// Send the built-in self-test notification (manual verification only).
    fn test(&self) -> Result<(), NotifyError> {
        self.notify(Notification {
            title: APP_DISPLAY_NAME.to_owned(),
            body: "If you can see this, Windows notifications are working for Rustboard."
                .to_owned(),
            tag: "self-test".to_owned(),
            urgent: false,
        })
    }
}

/// The shell/OS operation that actually shows a toast.
///
/// Split from [ToastNotifier] so tests can prove the fallback path without
/// touching the real notification system.
pub trait ToastBackend: Send + Sync {
    /// Show one toast, or return an error.
    fn show(&self, notification: &Notification) -> Result<(), NotifyError>;
}

/// The best-effort path used when registration or [ToastBackend::show] fails.
///
/// 170 implements this as a taskbar flash plus an in-app banner. Returning Ok
/// means the fallback delivered the reminder; returning Err means the reminder
/// was lost and [Notifier::notify] reports [NotifyError::FallbackFailed].
pub trait FallbackNotifier: Send + Sync {
    /// Report a failed toast to the fallback.
    fn notify_fallback(
        &self,
        notification: &Notification,
        error: &NotifyError,
    ) -> Result<(), NotifyError>;
}

/// Writes the Start Menu shortcut that carries the AUMID property.
pub trait ShortcutRegistrar: Send + Sync {
    /// Create or replace shortcut, pointing it at target and optionally icon.
    fn write_shortcut(
        &self,
        shortcut: &Path,
        target: &Path,
        icon: Option<&Path>,
    ) -> Result<(), NotifyError>;
}

/// Writes the per-user AppUserModelId registry key.
pub trait AppIdRegistrar: Send + Sync {
    /// Create or update the AUMID key for [AUMID].
    fn register_app_id(&self) -> Result<(), NotifyError>;
}

/// Paths used by AUMID registration.
///
/// Production resolves these from %APPDATA%; tests build a scratch layout with
/// [NotificationPaths::under_appdata] so no real Start Menu entry is touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPaths {
    /// Folder that receives Rustboard.lnk.
    pub start_menu_dir: PathBuf,
    /// Per-user roaming AppData root the paths were derived from.
    pub appdata_root: PathBuf,
}

impl NotificationPaths {
    /// Resolve %APPDATA%\Microsoft\Windows\Start Menu\Programs.
    pub fn production() -> Result<Self, NotifyError> {
        let appdata_root = std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(NotifyError::MissingAppData)?;
        Ok(Self::under_appdata(appdata_root))
    }

    /// Derive the Start Menu folder from an explicit AppData root.
    pub fn under_appdata(appdata_root: impl Into<PathBuf>) -> Self {
        let appdata_root = appdata_root.into();
        let start_menu_dir = appdata_root
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        Self {
            start_menu_dir,
            appdata_root,
        }
    }

    /// Build an explicit layout (test seam).
    pub fn new(start_menu_dir: impl Into<PathBuf>, appdata_root: impl Into<PathBuf>) -> Self {
        Self {
            start_menu_dir: start_menu_dir.into(),
            appdata_root: appdata_root.into(),
        }
    }

    /// Folder that receives the shortcut.
    pub fn start_menu_dir(&self) -> &Path {
        &self.start_menu_dir
    }

    /// Per-user roaming AppData root.
    pub fn appdata_root(&self) -> &Path {
        &self.appdata_root
    }

    /// Full path of Rustboard.lnk.
    pub fn shortcut_path(&self) -> PathBuf {
        self.start_menu_dir.join(SHORTCUT_FILE_NAME)
    }
}

/// The concrete registration side effects used by one [ToastNotifier].
///
/// Both halves are injectable so unit tests never write the real Start Menu or
/// the real registry.
pub struct Registration {
    shortcut_path: PathBuf,
    target: PathBuf,
    icon: Option<PathBuf>,
    shortcut: Arc<dyn ShortcutRegistrar>,
    app_id: Arc<dyn AppIdRegistrar>,
}

impl Registration {
    /// Production registration: the current executable, the shipped icon (when
    /// found) and the real shell/registry writers.
    pub fn production(paths: NotificationPaths) -> Result<Self, NotifyError> {
        let target = std::env::current_exe().map_err(NotifyError::CurrentExe)?;
        let icon = default_icon_path(&target);
        Ok(Self::with_parts(
            paths.shortcut_path(),
            target,
            icon,
            Arc::new(ShellShortcutRegistrar),
            Arc::new(ShellAppIdRegistrar),
        ))
    }

    /// Build registration from explicitly injected pieces (test seam).
    pub fn with_parts(
        shortcut_path: impl Into<PathBuf>,
        target: impl Into<PathBuf>,
        icon: Option<PathBuf>,
        shortcut: Arc<dyn ShortcutRegistrar>,
        app_id: Arc<dyn AppIdRegistrar>,
    ) -> Self {
        Self {
            shortcut_path: shortcut_path.into(),
            target: target.into(),
            icon,
            shortcut,
            app_id,
        }
    }

    /// Ensure both registrations exist. Idempotent; the notifier caches success
    /// process-wide by shortcut path.
    pub fn ensure(&self) -> Result<(), NotifyError> {
        self.shortcut
            .write_shortcut(&self.shortcut_path, &self.target, self.icon.as_deref())?;
        self.app_id.register_app_id()
    }

    /// Shortcut path this registration owns.
    pub fn shortcut_path(&self) -> &Path {
        &self.shortcut_path
    }

    /// Executable the shortcut launches.
    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// The real WinRT toast backend (tauri-winrt-notification).
#[derive(Clone, Copy, Debug, Default)]
pub struct WinRtBackend;

impl ToastBackend for WinRtBackend {
    fn show(&self, notification: &Notification) -> Result<(), NotifyError> {
        let title = match notification.title.trim() {
            "" => APP_DISPLAY_NAME,
            _ => notification.title.as_str(),
        };
        let body = match notification.body.trim() {
            "" => "Reminder",
            _ => notification.body.as_str(),
        };
        let duration = if notification.urgent {
            Duration::Long
        } else {
            Duration::Short
        };
        let sound = if notification.urgent {
            Sound::Reminder
        } else {
            Sound::Default
        };

        Toast::new(AUMID)
            .title(title)
            .text1(body)
            .duration(duration)
            .sound(Some(sound))
            .show()
            .map_err(NotifyError::Show)
    }
}

/// Default fallback: report that no visible fallback is installed yet.
///
/// 170 replaces this with taskbar flash plus an in-app banner. Until then a
/// failed toast is logged by [ToastNotifier::notify] and surfaces as
/// [NotifyError::FallbackFailed], so 140 knows the reminder was not delivered.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultFallback;

impl FallbackNotifier for DefaultFallback {
    fn notify_fallback(
        &self,
        _notification: &Notification,
        _error: &NotifyError,
    ) -> Result<(), NotifyError> {
        Err(NotifyError::Fallback {
            message: "no visible fallback is installed".to_owned(),
        })
    }
}

/// The real Start Menu shortcut writer (IShellLinkW + IPropertyStore).
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellShortcutRegistrar;

impl ShortcutRegistrar for ShellShortcutRegistrar {
    fn write_shortcut(
        &self,
        shortcut: &Path,
        target: &Path,
        icon: Option<&Path>,
    ) -> Result<(), NotifyError> {
        let parent = shortcut
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| NotifyError::InvalidShortcutPath {
                path: shortcut.to_path_buf(),
            })?;
        std::fs::create_dir_all(parent).map_err(|source| NotifyError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;

        let _apartment = ComApartment::initialize()?;
        // SAFETY: COM is initialized on this thread by _apartment; every COM
        // pointer below is used only on this thread and released by its smart
        // pointer. propvariant_from_str returns an owned VT_LPWSTR value that
        // PROPVARIANT::drop clears with PropVariantClear.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|source| NotifyError::ShellStep {
                    step: "CoCreateInstance(ShellLink)",
                    source,
                })?;
            link.SetPath(&HSTRING::from(target))
                .map_err(|source| NotifyError::ShellStep {
                    step: "IShellLinkW::SetPath",
                    source,
                })?;
            if let Some(parent) = target.parent() {
                link.SetWorkingDirectory(&HSTRING::from(parent))
                    .map_err(|source| NotifyError::ShellStep {
                        step: "IShellLinkW::SetWorkingDirectory",
                        source,
                    })?;
            }
            link.SetDescription(&HSTRING::from(APP_DISPLAY_NAME))
                .map_err(|source| NotifyError::ShellStep {
                    step: "IShellLinkW::SetDescription",
                    source,
                })?;
            if let Some(icon) = icon {
                link.SetIconLocation(&HSTRING::from(icon), 0)
                    .map_err(|source| NotifyError::ShellStep {
                        step: "IShellLinkW::SetIconLocation",
                        source,
                    })?;
            }

            let store: IPropertyStore = link.cast().map_err(|source| NotifyError::ShellStep {
                step: "QueryInterface(IPropertyStore)",
                source,
            })?;
            let app_id = propvariant_from_str(AUMID)?;
            store
                .SetValue(
                    std::ptr::from_ref(&PKEY_AppUserModel_ID),
                    std::ptr::from_ref(&app_id),
                )
                .map_err(|source| NotifyError::ShellStep {
                    step: "IPropertyStore::SetValue(AppUserModel.ID)",
                    source,
                })?;
            store.Commit().map_err(|source| NotifyError::ShellStep {
                step: "IPropertyStore::Commit",
                source,
            })?;

            let persist: IPersistFile = link.cast().map_err(|source| NotifyError::ShellStep {
                step: "QueryInterface(IPersistFile)",
                source,
            })?;
            persist
                .Save(&HSTRING::from(shortcut), true)
                .map_err(|source| NotifyError::ShellStep {
                    step: "IPersistFile::Save",
                    source,
                })?;
        }
        Ok(())
    }
}

/// The real HKCU AppUserModelId registry writer.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellAppIdRegistrar;

impl AppIdRegistrar for ShellAppIdRegistrar {
    fn register_app_id(&self) -> Result<(), NotifyError> {
        let key_path = app_id_registry_key();
        let mut raw_key = HKEY::default();
        // SAFETY: the subkey is a valid NUL-terminated HSTRING, raw_key points
        // to writable memory, and the returned handle is closed by RegKey.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key_path.as_str()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut raw_key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(NotifyError::Registry(status.into()));
        }
        let key = RegKey(raw_key);

        let mut display_name: Vec<u16> = APP_DISPLAY_NAME.encode_utf16().collect();
        display_name.push(0);
        // SAFETY: display_name is a live Vec<u16>; the slice borrows its bytes
        // only for the duration of the RegSetValueExW call.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                display_name.as_ptr().cast::<u8>(),
                display_name.len() * std::mem::size_of::<u16>(),
            )
        };
        let status = unsafe {
            RegSetValueExW(
                key.0,
                &HSTRING::from("DisplayName"),
                None,
                REG_SZ,
                Some(bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(NotifyError::Registry(status.into()));
        }
        Ok(())
    }
}

/// The toast notifier used by 140 and 170.
///
/// Keep one instance for the life of the process: successful registration is
/// cached process-wide by shortcut path, so the Start Menu shortcut and registry
/// key are written only on the first call.
pub struct ToastNotifier {
    backend: Arc<dyn ToastBackend>,
    fallback: Arc<dyn FallbackNotifier>,
    registration: Registration,
}

impl ToastNotifier {
    /// Production notifier: sets the process AUMID, resolves %APPDATA%, and
    /// installs the real WinRT backend with the default fallback.
    ///
    /// 170 can call [ensure_app_id] earlier for the tray/taskbar identity; that
    /// call is cached and safe to repeat. Install the real fallback with
    /// [ToastNotifier::with_fallback].
    pub fn new() -> Result<Self, NotifyError> {
        ensure_app_id()?;
        let paths = NotificationPaths::production()?;
        let registration = Registration::production(paths)?;
        Ok(Self::with_backend(Arc::new(WinRtBackend), registration))
    }

    /// Inject a toast backend (test seam).
    pub fn with_backend(backend: Arc<dyn ToastBackend>, registration: Registration) -> Self {
        Self::with_parts(backend, Arc::new(DefaultFallback), registration)
    }

    /// Inject every seam: backend, fallback and registration.
    pub fn with_parts(
        backend: Arc<dyn ToastBackend>,
        fallback: Arc<dyn FallbackNotifier>,
        registration: Registration,
    ) -> Self {
        Self {
            backend,
            fallback,
            registration,
        }
    }

    /// Replace the default log-only fallback (170 wires taskbar flash + banner).
    pub fn with_fallback(mut self, fallback: Arc<dyn FallbackNotifier>) -> Self {
        self.fallback = fallback;
        self
    }

    /// Ensure the AUMID shortcut and registry key exist, at most once per
    /// process and shortcut path after a success. A failure is returned and
    /// retried next time.
    pub fn ensure_registration(&self) -> Result<(), NotifyError> {
        static ENSURED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        let path = self.registration.shortcut_path().to_path_buf();
        let mut ensured = ENSURED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ensured.iter().any(|cached| cached == &path) {
            return Ok(());
        }
        self.registration.ensure()?;
        ensured.push(path);
        Ok(())
    }

    /// The registration this notifier uses.
    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    fn deliver_with_fallback(
        &self,
        notification: &Notification,
        primary: NotifyError,
    ) -> Result<(), NotifyError> {
        match self.fallback.notify_fallback(notification, &primary) {
            Ok(()) => Ok(()),
            Err(fallback) => Err(NotifyError::FallbackFailed {
                primary: Box::new(primary),
                fallback: Box::new(fallback),
            }),
        }
    }
}

impl Notifier for ToastNotifier {
    fn notify(&self, notification: Notification) -> Result<(), NotifyError> {
        if let Err(error) = self.ensure_registration() {
            log_failure(&notification, &error);
            return self.deliver_with_fallback(&notification, error);
        }

        match self.backend.show(&notification) {
            Ok(()) => Ok(()),
            Err(error) => {
                log_failure(&notification, &error);
                self.deliver_with_fallback(&notification, error)
            }
        }
    }
}

/// Set the process-wide explicit AppUserModelID once.
///
/// 170 calls this at startup so the tray and taskbar group under the same
/// identity as the toasts. Repeated calls after a success are no-ops; a failed
/// call is retried by the next caller.
pub fn ensure_app_id() -> Result<(), NotifyError> {
    static PROCESS_APP_ID_SET: AtomicBool = AtomicBool::new(false);
    if PROCESS_APP_ID_SET.load(Ordering::Acquire) {
        return Ok(());
    }

    let app_id = HSTRING::from(AUMID);
    // SAFETY: app_id is a valid NUL-terminated HSTRING. The API only reads it.
    unsafe { SetCurrentProcessExplicitAppUserModelID(&app_id) }
        .map_err(NotifyError::ProcessAppId)?;
    PROCESS_APP_ID_SET.store(true, Ordering::Release);
    Ok(())
}

/// Show the real self-test toast through the production notifier.
///
/// Manual verification only (for example a future --test-notification flag
/// wired by 170). Unit tests never call this because it writes real per-user
/// registration state.
pub fn test_notification() -> Result<(), NotifyError> {
    ToastNotifier::new()?.test()
}

/// Production fallback: log the failed delivery.
fn log_failure(notification: &Notification, error: &NotifyError) {
    let tag = if notification.tag.is_empty() {
        "-"
    } else {
        notification.tag.as_str()
    };
    eprintln!("rustboard: toast notification for tag {tag} failed: {error}");
}

/// Full HKCU key that carries the AUMID display name.
fn app_id_registry_key() -> String {
    format!(r"Software\Classes\AppUserModelId\{AUMID}")
}

/// Find the shipped icon next to the executable, or in the source tree.
fn default_icon_path(exe: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("assets").join(ICON_FILE_NAME));
        candidates.push(dir.join(ICON_FILE_NAME));
    }
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("assets").join(ICON_FILE_NAME));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Owned VT_LPWSTR property value allocated with the COM task allocator.
fn propvariant_from_str(value: &str) -> Result<PROPVARIANT, NotifyError> {
    let mut wide: Vec<u16> = value.encode_utf16().collect();
    wide.push(0);
    let bytes = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or(NotifyError::OutOfMemory)?;

    // SAFETY: CoTaskMemAlloc returns uninitialized memory from the COM allocator.
    // PROPVARIANT::drop releases VT_LPWSTR values with PropVariantClear.
    let raw = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if raw.is_null() {
        return Err(NotifyError::OutOfMemory);
    }
    // SAFETY: raw points to bytes writable bytes and wide occupies exactly bytes
    // bytes, so the non-overlapping copy stays in bounds.
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), raw, wide.len()) };

    Ok(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(raw),
                },
            }),
        },
    })
}

/// RAII guard for a per-thread COM apartment.
struct ComApartment {
    uninitialize: bool,
}

impl ComApartment {
    fn initialize() -> Result<Self, NotifyError> {
        // SAFETY: per-thread COM initialization; the guard uninitializes on Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == S_OK {
            Ok(Self { uninitialize: true })
        } else if hr == S_FALSE {
            Ok(Self {
                uninitialize: false,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            // Another library already initialized this thread with a different
            // apartment model; shell COM calls still work from here.
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(NotifyError::ComInit(hr.into()))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balanced with the successful CoInitializeEx in initialize.
            unsafe { CoUninitialize() };
        }
    }
}

/// RAII guard for an HKEY.
struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: self.0 came from RegCreateKeyExW and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::thread::ThreadId;

    use super::*;

    /// Scratch folder inside the checkout, as required by the project rules.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!(
                "rustboard-notify-{name}-{}-{serial}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create notify scratch dir");
        dir
    }

    fn test_target() -> PathBuf {
        std::env::current_exe().expect("test executable path")
    }

    #[derive(Default)]
    struct FakeBackend {
        fail: AtomicBool,
        calls: AtomicUsize,
        seen: Mutex<Vec<Notification>>,
        thread: Mutex<Option<ThreadId>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            let backend = Self::default();
            backend.fail.store(true, Ordering::SeqCst);
            backend
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn seen(&self) -> Vec<Notification> {
            self.seen.lock().expect("fake backend seen").clone()
        }

        fn thread(&self) -> Option<ThreadId> {
            *self.thread.lock().expect("fake backend thread")
        }
    }

    impl ToastBackend for FakeBackend {
        fn show(&self, notification: &Notification) -> Result<(), NotifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .expect("fake backend seen")
                .push(notification.clone());
            *self.thread.lock().expect("fake backend thread") = Some(std::thread::current().id());
            if self.fail.load(Ordering::SeqCst) {
                Err(NotifyError::Backend {
                    message: "fake backend failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FakeFallback {
        fail: AtomicBool,
        calls: AtomicUsize,
        seen: Mutex<Vec<(Notification, String)>>,
    }

    impl FakeFallback {
        fn new() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            let fallback = Self::default();
            fallback.fail.store(true, Ordering::SeqCst);
            fallback
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_error(&self) -> String {
            self.seen
                .lock()
                .expect("fake fallback seen")
                .last()
                .map(|(_, error)| error.clone())
                .unwrap_or_default()
        }
    }

    impl FallbackNotifier for FakeFallback {
        fn notify_fallback(
            &self,
            notification: &Notification,
            error: &NotifyError,
        ) -> Result<(), NotifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .expect("fake fallback seen")
                .push((notification.clone(), error.to_string()));
            if self.fail.load(Ordering::SeqCst) {
                Err(NotifyError::Fallback {
                    message: "fake fallback failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FakeShortcutRegistrar {
        fail: AtomicBool,
        calls: AtomicUsize,
    }

    impl FakeShortcutRegistrar {
        fn new() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            let registrar = Self::default();
            registrar.fail.store(true, Ordering::SeqCst);
            registrar
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ShortcutRegistrar for FakeShortcutRegistrar {
        fn write_shortcut(
            &self,
            _shortcut: &Path,
            _target: &Path,
            _icon: Option<&Path>,
        ) -> Result<(), NotifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(NotifyError::Registration {
                    message: "fake shortcut failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FakeAppIdRegistrar {
        fail: AtomicBool,
        calls: AtomicUsize,
    }

    impl FakeAppIdRegistrar {
        fn new() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            let registrar = Self::default();
            registrar.fail.store(true, Ordering::SeqCst);
            registrar
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AppIdRegistrar for FakeAppIdRegistrar {
        fn register_app_id(&self) -> Result<(), NotifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(NotifyError::Registration {
                    message: "fake app id failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    fn registration_with(
        name: &str,
        shortcut: Arc<dyn ShortcutRegistrar>,
        app_id: Arc<dyn AppIdRegistrar>,
        target: PathBuf,
    ) -> Registration {
        let paths = NotificationPaths::under_appdata(scratch(name));
        Registration::with_parts(paths.shortcut_path(), target, None, shortcut, app_id)
    }

    fn fake_notifier(
        name: &str,
        backend: Arc<FakeBackend>,
        fallback: Arc<FakeFallback>,
    ) -> ToastNotifier {
        let registration = registration_with(
            name,
            Arc::new(FakeShortcutRegistrar::new()),
            Arc::new(FakeAppIdRegistrar::new()),
            PathBuf::from(r"C:\fake\Rustboard.exe"),
        );
        ToastNotifier::with_parts(backend, fallback, registration)
    }

    #[test]
    fn aumid_and_names_are_stable() {
        assert_eq!(AUMID, "com.rustboard.app");
        assert_eq!(APP_DISPLAY_NAME, "Rustboard");
        assert_eq!(SHORTCUT_FILE_NAME, "Rustboard.lnk");
        assert_eq!(ICON_FILE_NAME, "rustboard.ico");
    }

    #[test]
    fn notification_is_plain_data() {
        let notification = Notification::new("Title", "Body")
            .with_tag("reminder-7")
            .urgent();
        assert_eq!(notification.title, "Title");
        assert_eq!(notification.body, "Body");
        assert_eq!(notification.tag, "reminder-7");
        assert!(notification.urgent);
        assert_eq!(notification.clone(), notification);
    }

    #[test]
    fn paths_derive_the_shortcut_from_the_appdata_root() {
        let root = scratch("paths");
        let paths = NotificationPaths::under_appdata(root.as_path());
        assert_eq!(paths.appdata_root(), root.as_path());
        assert!(paths.start_menu_dir().ends_with("Programs"));
        assert!(paths.shortcut_path().ends_with(SHORTCUT_FILE_NAME));
        assert_eq!(
            paths.shortcut_path(),
            paths.start_menu_dir().join(SHORTCUT_FILE_NAME)
        );
    }

    #[cfg(windows)]
    #[test]
    fn production_paths_live_under_appdata() {
        let paths = NotificationPaths::production().expect("APPDATA is set");
        assert!(paths.start_menu_dir().starts_with(paths.appdata_root()));
        assert!(paths.shortcut_path().ends_with(SHORTCUT_FILE_NAME));
    }

    #[test]
    fn a_delivered_toast_does_not_use_the_fallback() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let notifier = fake_notifier("success", backend.clone(), fallback.clone());

        notifier
            .notify(Notification::new("Title", "Body"))
            .expect("toast succeeds");

        assert_eq!(backend.calls(), 1);
        assert_eq!(fallback.calls(), 0);
    }

    #[test]
    fn a_backend_error_is_reported_through_the_fallback() {
        let backend = Arc::new(FakeBackend::failing());
        let fallback = Arc::new(FakeFallback::new());
        let notifier = fake_notifier("backend-fallback", backend.clone(), fallback.clone());

        notifier
            .notify(Notification::new("Title", "Body").with_tag("tag-1"))
            .expect("fallback delivers the reminder");

        assert_eq!(backend.calls(), 1);
        assert_eq!(fallback.calls(), 1);
        assert!(fallback.last_error().contains("fake backend failure"));
    }

    #[test]
    fn a_failed_toast_and_failed_fallback_returns_fallback_failed() {
        let backend = Arc::new(FakeBackend::failing());
        let fallback = Arc::new(FakeFallback::failing());
        let notifier = fake_notifier("both-fail", backend, fallback.clone());

        let error = notifier
            .notify(Notification::new("Title", "Body"))
            .expect_err("both paths fail");
        assert!(matches!(error, NotifyError::FallbackFailed { .. }));
        assert_eq!(fallback.calls(), 1);
    }

    #[test]
    fn registration_failure_uses_the_fallback_without_showing_a_toast() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let registration = registration_with(
            "registration-fallback",
            Arc::new(FakeShortcutRegistrar::failing()),
            Arc::new(FakeAppIdRegistrar::new()),
            PathBuf::from(r"C:\fake\Rustboard.exe"),
        );
        let notifier = ToastNotifier::with_parts(backend.clone(), fallback.clone(), registration);

        notifier
            .notify(Notification::new("Title", "Body"))
            .expect("fallback delivers the reminder");

        assert_eq!(backend.calls(), 0);
        assert_eq!(fallback.calls(), 1);
        assert!(fallback.last_error().contains("fake shortcut failure"));
    }

    #[test]
    fn app_id_registration_failure_uses_the_fallback_without_showing_a_toast() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let shortcut = Arc::new(FakeShortcutRegistrar::new());
        let registration = registration_with(
            "app-id-failure",
            shortcut.clone(),
            Arc::new(FakeAppIdRegistrar::failing()),
            PathBuf::from(r"C:\fake\Rustboard.exe"),
        );
        let notifier = ToastNotifier::with_parts(backend.clone(), fallback.clone(), registration);

        notifier
            .notify(Notification::new("Title", "Body"))
            .expect("fallback delivers the reminder");

        assert_eq!(shortcut.calls(), 1);
        assert_eq!(backend.calls(), 0);
        assert_eq!(fallback.calls(), 1);
        assert!(fallback.last_error().contains("fake app id failure"));
    }

    #[test]
    fn app_id_registry_key_contains_the_aumid() {
        assert_eq!(
            app_id_registry_key(),
            r"Software\Classes\AppUserModelId\com.rustboard.app"
        );
    }

    #[test]
    fn registration_is_attempted_once_after_success() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let shortcut = Arc::new(FakeShortcutRegistrar::new());
        let app_id = Arc::new(FakeAppIdRegistrar::new());
        let registration = registration_with(
            "once",
            shortcut.clone(),
            app_id.clone(),
            PathBuf::from(r"C:\fake\Rustboard.exe"),
        );
        let notifier = ToastNotifier::with_parts(backend.clone(), fallback, registration);

        notifier.ensure_registration().expect("first");
        notifier.ensure_registration().expect("second");
        notifier
            .notify(Notification::new("Title", "Body"))
            .expect("toast succeeds");

        assert_eq!(shortcut.calls(), 1);
        assert_eq!(app_id.calls(), 1);
        assert_eq!(backend.calls(), 1);
    }

    #[test]
    fn notify_stays_on_the_calling_thread() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let notifier = fake_notifier("thread", backend.clone(), fallback);
        let caller = std::thread::current().id();

        notifier
            .notify(Notification::new("Title", "Body"))
            .expect("toast succeeds");

        assert_eq!(backend.thread(), Some(caller));
    }

    #[test]
    fn test_sends_the_self_test_notification() {
        let backend = Arc::new(FakeBackend::new());
        let fallback = Arc::new(FakeFallback::new());
        let notifier = fake_notifier("self-test", backend.clone(), fallback.clone());

        notifier.test().expect("self-test toast succeeds");

        assert_eq!(fallback.calls(), 0);
        let seen = backend.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].title, APP_DISPLAY_NAME);
        assert_eq!(seen[0].tag, "self-test");
    }

    #[cfg(windows)]
    #[test]
    fn registration_creates_one_shortcut_and_the_second_call_is_a_noop() {
        struct CountingShortcut(ShellShortcutRegistrar, AtomicUsize);

        impl ShortcutRegistrar for CountingShortcut {
            fn write_shortcut(
                &self,
                shortcut: &Path,
                target: &Path,
                icon: Option<&Path>,
            ) -> Result<(), NotifyError> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.write_shortcut(shortcut, target, icon)
            }
        }

        let paths = NotificationPaths::under_appdata(scratch("registration"));
        let shortcut = Arc::new(CountingShortcut(
            ShellShortcutRegistrar,
            AtomicUsize::new(0),
        ));
        let app_id = Arc::new(FakeAppIdRegistrar::new());
        let registration = Registration::with_parts(
            paths.shortcut_path(),
            test_target(),
            None,
            shortcut.clone(),
            app_id.clone(),
        );
        let notifier = ToastNotifier::with_backend(Arc::new(FakeBackend::new()), registration);

        notifier.ensure_registration().expect("first registration");
        notifier.ensure_registration().expect("second registration");

        assert_eq!(shortcut.1.load(Ordering::SeqCst), 1);
        assert_eq!(app_id.calls(), 1);
        assert!(paths.shortcut_path().is_file());

        let links = std::fs::read_dir(paths.start_menu_dir())
            .expect("read start menu folder")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
            })
            .count();
        assert_eq!(links, 1);
    }

    #[cfg(windows)]
    #[test]
    fn shortcut_stores_the_aumid_property() {
        let paths = NotificationPaths::under_appdata(scratch("shortcut-aumid"));
        ShellShortcutRegistrar
            .write_shortcut(&paths.shortcut_path(), &test_target(), None)
            .expect("write shortcut");

        assert!(paths.shortcut_path().is_file());
        assert_eq!(
            read_shortcut_aumid(&paths.shortcut_path()).expect("read AUMID property"),
            AUMID
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_failing_registration_is_reported_not_panicked() {
        let root = scratch("registration-failure");
        let blocker = root.join("blocked");
        std::fs::write(&blocker, b"not a directory").expect("write blocker");
        let registration = Registration::with_parts(
            blocker.join(SHORTCUT_FILE_NAME),
            test_target(),
            None,
            Arc::new(ShellShortcutRegistrar),
            Arc::new(FakeAppIdRegistrar::new()),
        );
        let notifier = ToastNotifier::with_backend(Arc::new(FakeBackend::new()), registration);

        let error = notifier
            .ensure_registration()
            .expect_err("shortcut parent is a file");
        assert!(
            matches!(error, NotifyError::CreateDir { .. }),
            "unexpected error: {error}"
        );
    }

    /// Read the AUMID property back through IShellLinkW/IPropertyStore.
    #[cfg(windows)]
    fn read_shortcut_aumid(path: &Path) -> Result<String, NotifyError> {
        use windows::Win32::System::Com::STGM_READ;
        use windows::core::BSTR;

        let _apartment = ComApartment::initialize()?;
        // SAFETY: COM is initialized on this thread; loaded interfaces are used
        // only here and released by their smart pointers.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let persist: IPersistFile = link.cast()?;
            persist.Load(&HSTRING::from(path), STGM_READ)?;
            let store: IPropertyStore = link.cast()?;
            let value = store.GetValue(std::ptr::from_ref(&PKEY_AppUserModel_ID))?;
            let text = BSTR::try_from(&value)?;
            Ok(text.to_string())
        }
    }
}
