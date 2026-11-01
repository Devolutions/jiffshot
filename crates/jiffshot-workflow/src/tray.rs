//! The resident part of the capture side: a hidden window on its own thread
//! that owns the global hotkeys, the timers and the capture workflow. The
//! app's Avalonia tray icon sends it commands ([`TrayAction`]) and shows
//! what it publishes ([`TrayState`], notices).

use crate::geometry::CaptureLayout;
#[cfg(target_os = "macos")]
use crate::geometry::{self, saved_area_layout, saved_area_ticket, SavedAreaKind, SavedAreaTicket};
use crate::keys;
use crate::overlay::{self, Overlay};
use crate::settings::Settings;
use crate::sys::{
    self, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, PostMessageW, PostQuitMessage,
    RegisterHotKey, SetTimer, TranslateMessage, UnregisterHotKey, HOT_KEY_MODIFIERS, HWND, LPARAM,
    LRESULT, MOD_NOREPEAT, MSG, WM_APP, WM_DESTROY, WM_HOTKEY, WM_TIMER, WPARAM,
};
use crate::workflow::{self, Capture, Outcome};
use crate::{log, modes};
use jiffshot_capture::library::{CaptureKind, Library};
use jiffshot_capture::screen::{self, CursorSnapshot};
use jiffshot_capture::targets::Targets;
use jiffshot_capture::Rect;
use std::cell::RefCell;
use std::time::Instant;

pub const CLASS: &str = "JiffshotCaptureTray";
/// Posted by a second instance (or a tray click) to start a capture.
pub const WM_APP_CAPTURE: u32 = WM_APP + 2;
const WM_APP_DONE: u32 = WM_APP + 3;
/// Posted (by a second instance) to capture open menus after a delay.
pub const WM_APP_MENU: u32 = WM_APP + 4;
/// A capture (wparam = mode) to start after lparam milliseconds, so the
/// window that asked can get out of the way.
pub const WM_APP_CAPTURE_LATER: u32 = WM_APP + 5;
/// A tray menu command (wparam = an `ID_*`), from [`TrayAction::post`].
const WM_APP_COMMAND: u32 = WM_APP + 6;
/// A closure to run on this thread (lparam = `Box<Box<dyn FnOnce()>>`), from
/// [`crate::on_capture_thread`].
pub(crate) const WM_APP_RUN: u32 = WM_APP + 7;
#[cfg(target_os = "macos")]
pub(crate) const WM_APP_REQUEST_EXIT: u32 = WM_APP + 8;
#[cfg(target_os = "macos")]
const WM_APP_SCROLL_DONE: u32 = WM_APP + 9;
const TIMER_DELAY: usize = 1;
const TIMER_HOTKEY_RETRY: usize = 2;
const TIMER_MENU: usize = 3;
const TIMER_INITIAL: usize = 4;
#[cfg(target_os = "macos")]
const TIMER_STARTUP_STATUS: usize = 9;

/// What a capture produces, passed as WM_APP_CAPTURE's wparam.
pub const MODE_IMAGE: usize = 0;
pub const MODE_SCROLLING: usize = 1;
pub const MODE_PANORAMIC: usize = 2;
pub const MODE_TEXT: usize = 3;
pub const MODE_VIDEO: usize = 4;
pub const MODE_FREEHAND: usize = 5;
pub const MODE_MULTI: usize = 6;
pub const MODE_FIXED: usize = 7;
pub const MODE_SCROLL_H: usize = 8;
pub const MODE_STEPS: usize = 9;
pub const MODE_SCROLL_BOTH: usize = 10;
/// Select an area, then choose image, video, scrolling or panoramic.
pub const MODE_ALL: usize = 11;
pub const MODE_WINDOW: usize = 12;
pub const MODE_FULLSCREEN: usize = 13;
pub const MODE_CLIPBOARD: usize = 14;
pub const MODE_FILE: usize = 15;
pub const MODE_VIDEO_FULL: usize = 16;
#[cfg(target_os = "macos")]
const MODE_SCHEDULE_AREA: usize = 17;
/// Not a mode: the command line asks for the Capture window.
pub const OPEN_CAPTURE_WINDOW: usize = 99;
const ID_SCROLL_BOTH: usize = 48;
const ID_FROM_CLIPBOARD: usize = 49;
const ID_FROM_FILE: usize = 50;
const ID_SCHEDULE: usize = 51;
const TIMER_SCHEDULE: usize = 7;
const TIMER_MODE: usize = 8;
const ID_CAPWIN: usize = 52;
const TIMER_TIMELAPSE: usize = 5;
const TIMER_PRESET: usize = 6;
const HOTKEY_PRESET: i32 = 100;
const ID_FREEHAND: usize = 40;
const ID_MULTI: usize = 41;
const ID_FIXED: usize = 42;
const ID_SCROLL_H: usize = 43;
const ID_STEPS: usize = 44;
const ID_TIMELAPSE: usize = 45;
const ID_WIDGET: usize = 46;
const ID_STARTUP: usize = 47;
const ID_PRESET: usize = 100;

/// The capture mode a preset's `mode` names.
pub fn mode_named(name: &str) -> Option<usize> {
    Some(match name {
        "all" => MODE_ALL,
        "image" => MODE_IMAGE,
        "window" => MODE_WINDOW,
        "fullscreen" => MODE_FULLSCREEN,
        "clipboard" => MODE_CLIPBOARD,
        "file" => MODE_FILE,
        "video-fullscreen" => MODE_VIDEO_FULL,
        "scrolling" => MODE_SCROLLING,
        "horizontal" => MODE_SCROLL_H,
        "both" => MODE_SCROLL_BOTH,
        "panoramic" => MODE_PANORAMIC,
        "freehand" => MODE_FREEHAND,
        "multi" => MODE_MULTI,
        "fixed" => MODE_FIXED,
        "text" => MODE_TEXT,
        "video" => MODE_VIDEO,
        "steps" => MODE_STEPS,
        "menu" => usize::MAX,
        _ => return None,
    })
}

/// Parses "Ctrl+Shift+1" into hotkey modifiers and a virtual key.
pub fn parse_hotkey(text: &str) -> Option<(HOT_KEY_MODIFIERS, u16)> {
    keys::parse(text)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartupNotice {
    #[default]
    None,
    NeedsApproval,
    Legacy,
    StatusUnknown,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupStatus {
    /// Registration intent, including a login item awaiting user approval.
    pub requested: bool,
    pub notice: StartupNotice,
    pub detail: Option<String>,
}

pub(crate) fn startup_status() -> StartupStatus {
    startup::status()
}

pub(crate) fn runs_at_startup() -> bool {
    startup_status().requested
}

pub(crate) fn set_run_at_startup(on: bool) {
    #[cfg(windows)]
    startup::set_run_at_startup(on);
    #[cfg(target_os = "macos")]
    match startup::set_run_at_startup(on) {
        Ok(Some(message)) => notify(
            std::ptr::null_mut(),
            "Open at login needs approval",
            &message,
            true,
        ),
        Ok(None) => {}
        Err(error) => notify(
            std::ptr::null_mut(),
            "Open at login not changed",
            &error,
            true,
        ),
    }
}

/// Starting with the session: the Run key on Windows.
#[cfg(windows)]
mod startup {
    use jiffshot_capture::wide;
    use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;

    /// The Run key entry that starts the tray with Windows.
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    pub(crate) fn runs_at_startup() -> bool {
        run_value_exists(jiffshot_capture::paths::NAME)
    }

    pub(crate) fn status() -> super::StartupStatus {
        super::StartupStatus {
            requested: runs_at_startup(),
            ..Default::default()
        }
    }

    fn run_value_exists(name: &str) -> bool {
        let mut size = 0u32;
        // SAFETY: a size query of one registry value.
        unsafe {
            windows_sys::Win32::System::Registry::RegGetValueW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                wide(name).as_ptr(),
                windows_sys::Win32::System::Registry::RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            ) == 0
        }
    }

    pub(crate) fn set_run_at_startup(on: bool) {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegSetValueExW, HKEY, REG_SZ,
        };
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: opens, writes or deletes one value under our own user's Run key.
        unsafe {
            if RegCreateKeyW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), &mut key) != 0 {
                return;
            }
            if on {
                let exe = std::env::current_exe()
                    .map(|p| format!("\"{}\"", p.display()))
                    .unwrap_or_default();
                let value = wide(&exe);
                RegSetValueExW(
                    key,
                    wide(jiffshot_capture::paths::NAME).as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr() as *const u8,
                    (value.len() * 2) as u32,
                );
            } else {
                RegDeleteValueW(key, wide(jiffshot_capture::paths::NAME).as_ptr());
            }
            RegCloseKey(key);
        }
    }
}

#[cfg(target_os = "macos")]
#[path = "startup_macos.rs"]
mod startup;

const HOTKEY_CAPTURE: i32 = 1;
const HOTKEY_SCREEN: i32 = 2;
const HOTKEY_REPEAT: i32 = 3;

const ID_CAPTURE: usize = 1;
const ID_DELAYED: usize = 2;
const ID_SCREEN: usize = 3;
const ID_REPEAT: usize = 4;
const ID_SCROLLING: usize = 5;
const ID_PANORAMIC: usize = 6;
const ID_MENU: usize = 7;
const ID_TEXT: usize = 8;
const ID_VIDEO: usize = 9;
const ID_CLIPBOARD: usize = 10;
const ID_EDITOR: usize = 11;
const ID_CURSOR: usize = 12;
const ID_LIBRARY: usize = 20;
const ID_EXIT: usize = 30;

#[derive(Clone)]
struct CaptureRegion {
    rect: Rect,
    layout: CaptureLayout,
}

#[derive(Clone)]
struct TimeLapse {
    region: CaptureRegion,
    last_hash: u64,
}

enum PendingMode {
    Mode(usize),
    #[cfg(windows)]
    Fixed(Rect),
    #[cfg(target_os = "macos")]
    Fixed(SavedAreaTicket),
}

#[cfg(target_os = "macos")]
struct DeferredScheduledCapture {
    at: String,
    area: Option<(i32, i32, i32, i32)>,
    ticket: Option<SavedAreaTicket>,
    queued: std::time::Instant,
}

#[cfg(target_os = "macos")]
impl DeferredScheduledCapture {
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

    fn new(settings: &Settings, queued: std::time::Instant) -> std::io::Result<Self> {
        let at = settings
            .schedule
            .clone()
            .ok_or_else(|| std::io::Error::other("the scheduled capture is disabled"))?;
        let ticket = settings
            .schedule_area
            .map(|(x, y, width, height)| {
                saved_area_ticket(SavedAreaKind::Scheduled, Rect::new(x, y, width, height))
            })
            .transpose()?;
        Ok(Self {
            at,
            area: settings.schedule_area,
            ticket,
            queued,
        })
    }

    fn validate(&self, settings: &Settings, now: std::time::Instant) -> std::io::Result<()> {
        if settings.schedule.as_deref() != Some(self.at.as_str())
            || settings.schedule_area != self.area
        {
            return Err(std::io::Error::other(
                "the scheduled time or area changed while the capture was waiting",
            ));
        }
        if now.saturating_duration_since(self.queued) > Self::MAX_WAIT {
            return Err(std::io::Error::other(
                "another capture kept the schedule waiting for more than two minutes",
            ));
        }
        if let Some(ticket) = &self.ticket {
            ticket.validate()?;
        }
        Ok(())
    }
}

struct App {
    hwnd: HWND,
    settings: Settings,
    busy: bool,
    last_region: Option<CaptureRegion>,
    /// How the capture hotkey reads in menus, e.g. "PrtScn".
    capture_key: String,
    /// A capture requested on the command line, started once the tray runs.
    pending: Option<usize>,
    timelapse: Option<TimeLapse>,
    /// A preset waiting for its delay.
    pending_preset: Option<usize>,
    /// Overrides for the capture running now (from a preset).
    override_cursor: Option<bool>,
    override_effects: Option<Vec<String>>,
    override_share: Option<String>,
    scheduled_fired: Option<std::time::Instant>,
    #[cfg(target_os = "macos")]
    schedule_armed: bool,
    #[cfg(target_os = "macos")]
    deferred_schedule: Option<DeferredScheduledCapture>,
    /// A capture from the Capture window waiting for its delay: a mode, or
    /// a fixed rectangle.
    pending_mode: Option<PendingMode>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    #[cfg(target_os = "macos")]
    static PUBLISHED_STARTUP: RefCell<Option<StartupStatus>> = const { RefCell::new(None) };
}

/// Runs `f` with the app state. Never call it across a nested message loop:
/// the overlay re-enters the window procedure.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|app| app.borrow_mut().as_mut().map(f))
}

/// A notice for the user: the app shows it as a toast (only logged when no
/// app hosts the capture side).
fn notify(_hwnd: HWND, title: &str, text: &str, warning: bool) {
    match crate::hooks() {
        Some(hooks) => (hooks.notify)(title, text, warning),
        None => log(&format!("{title}: {text}")),
    }
}

/// What the tray menu shows, as the capture side sees it now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayState {
    pub copy_to_clipboard: bool,
    pub open_in_editor: bool,
    pub include_cursor: bool,
    pub show_widget: bool,
    pub run_at_startup: bool,
    pub startup_notice: StartupNotice,
    pub time_lapse: bool,
    pub schedule: bool,
    /// The user's schedule remains enabled but is not currently armed.
    pub schedule_paused: bool,
    /// A region was captured, so Repeat has something to repeat.
    pub can_repeat: bool,
    /// The capture hotkey, as the menu shows it ("PrtScn" or a fallback).
    pub capture_key: String,
}

/// Sends the current [`TrayState`] to the app.
pub(crate) fn publish_state() {
    let Some(hooks) = crate::hooks() else { return };
    let startup = startup_status();
    #[cfg(target_os = "macos")]
    remember_startup(&startup);
    let Some((state, settings)) = with_app(|app| {
        (
            TrayState {
                copy_to_clipboard: app.settings.copy_to_clipboard,
                open_in_editor: app.settings.open_in_editor,
                include_cursor: app.settings.include_cursor,
                show_widget: app.settings.show_widget,
                run_at_startup: startup.requested,
                startup_notice: startup.notice,
                time_lapse: app.timelapse.is_some(),
                schedule: app.settings.schedule.is_some(),
                #[cfg(target_os = "macos")]
                schedule_paused: app.settings.schedule.is_some() && !app.schedule_armed,
                #[cfg(windows)]
                schedule_paused: false,
                can_repeat: app.last_region.is_some(),
                capture_key: app.capture_key.clone(),
            },
            app.settings.clone(),
        )
    }) else {
        return;
    };
    #[cfg(not(target_os = "macos"))]
    {
        *crate::SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings);
    }
    #[cfg(target_os = "macos")]
    {
        *crate::CAPTURE_SNAPSHOT
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((settings, state.clone()));
    }
    (hooks.state)(state);
}

#[cfg(target_os = "macos")]
fn remember_startup(status: &StartupStatus) -> bool {
    PUBLISHED_STARTUP.with(|published| {
        let mut published = published.borrow_mut();
        if published.as_ref() == Some(status) {
            false
        } else {
            *published = Some(status.clone());
            true
        }
    })
}

#[cfg(target_os = "macos")]
fn refresh_startup_status() {
    let status = startup_status();
    if remember_startup(&status) {
        if let Some((_, state)) = crate::CAPTURE_SNAPSHOT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            state.run_at_startup = status.requested;
            state.startup_notice = status.notice;
        }
        if let Some(hooks) = crate::hooks() {
            (hooks.startup_status)(status);
        }
    }
}

/// The tray menu's commands, run on the capture side's thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Capture,
    CaptureDelayed,
    CaptureScreen,
    RepeatLast,
    CaptureWindow,
    Scrolling,
    ScrollingHorizontal,
    ScrollingBoth,
    Panoramic,
    Freehand,
    MultipleAreas,
    FixedSize,
    MenuCapture,
    GrabText,
    StepCapture,
    FromClipboard,
    FromFile,
    RecordVideo,
    TimeLapse,
    Schedule,
    CopyToClipboard,
    OpenInEditor,
    IncludeCursor,
    ShowWidget,
    RunAtStartup,
    OpenLibraryFolder,
    Exit,
}

impl TrayAction {
    fn id(self) -> usize {
        match self {
            TrayAction::Capture => ID_CAPTURE,
            TrayAction::CaptureDelayed => ID_DELAYED,
            TrayAction::CaptureScreen => ID_SCREEN,
            TrayAction::RepeatLast => ID_REPEAT,
            TrayAction::CaptureWindow => ID_CAPWIN,
            TrayAction::Scrolling => ID_SCROLLING,
            TrayAction::ScrollingHorizontal => ID_SCROLL_H,
            TrayAction::ScrollingBoth => ID_SCROLL_BOTH,
            TrayAction::Panoramic => ID_PANORAMIC,
            TrayAction::Freehand => ID_FREEHAND,
            TrayAction::MultipleAreas => ID_MULTI,
            TrayAction::FixedSize => ID_FIXED,
            TrayAction::MenuCapture => ID_MENU,
            TrayAction::GrabText => ID_TEXT,
            TrayAction::StepCapture => ID_STEPS,
            TrayAction::FromClipboard => ID_FROM_CLIPBOARD,
            TrayAction::FromFile => ID_FROM_FILE,
            TrayAction::RecordVideo => ID_VIDEO,
            TrayAction::TimeLapse => ID_TIMELAPSE,
            TrayAction::Schedule => ID_SCHEDULE,
            TrayAction::CopyToClipboard => ID_CLIPBOARD,
            TrayAction::OpenInEditor => ID_EDITOR,
            TrayAction::IncludeCursor => ID_CURSOR,
            TrayAction::ShowWidget => ID_WIDGET,
            TrayAction::RunAtStartup => ID_STARTUP,
            TrayAction::OpenLibraryFolder => ID_LIBRARY,
            TrayAction::Exit => ID_EXIT,
        }
    }

    /// Runs the command on the capture side's thread; safe from any thread.
    pub fn post(self) {
        sys::post_to_class(CLASS, WM_APP_COMMAND, self.id(), 0);
    }
}

fn register_hotkey(hwnd: HWND, id: i32, modifiers: HOT_KEY_MODIFIERS, vk: u16) -> bool {
    // SAFETY: registers a hotkey for our own window.
    unsafe { RegisterHotKey(hwnd, id, modifiers | MOD_NOREPEAT, vk as u32) != 0 }
}

/// Whether Windows routes PrtScn to the Snipping Tool (Settings ›
/// Accessibility › Keyboard).
#[cfg(windows)]
fn snipping_owns_print_screen() -> bool {
    use jiffshot_capture::wide;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 0;
    let mut size = 4u32;
    // SAFETY: reads one DWORD into a correctly sized buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide("Control Panel\\Keyboard").as_ptr(),
            wide("PrintScreenKeyForSnippingEnabled").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut _,
            &mut size,
        )
    };
    status == 0 && value != 0
}

#[cfg(not(windows))]
fn snipping_owns_print_screen() -> bool {
    false
}

fn hotkey_owner(key: &str) -> &'static str {
    if key == "PrtScn" && snipping_owns_print_screen() {
        "Windows gives it to the Snipping Tool (Settings › Accessibility › Keyboard › Print screen key)"
    } else {
        "another app or the system holds it"
    }
}

fn configured_key(
    name: &str,
    fallback: keys::Key,
    label: &str,
    problems: &mut Vec<String>,
) -> (HOT_KEY_MODIFIERS, u16, String) {
    match keys::parse(name).filter(|(mods, vk)| keys::supported(*vk) && !keys::reserved(*mods, *vk))
    {
        Some((mods, vk)) => (mods, vk, name.to_owned()),
        None => {
            problems.push(format!(
                "{name} is not a supported {label} hotkey; using {}",
                keys::display(fallback.2)
            ));
            (fallback.0, fallback.1, fallback.2.to_owned())
        }
    }
}

/// Registers the configured capture key, or a free fallback, plus the
/// full-screen and repeat keys.
fn register_hotkeys(hwnd: HWND, settings: &Settings) -> (String, Vec<String>) {
    let mut problems = Vec::new();
    let primary = configured_key(
        settings.capture_key(),
        keys::CAPTURE,
        "capture",
        &mut problems,
    );
    let screen = configured_key(
        settings.screen_key(),
        keys::SCREEN,
        "full-screen",
        &mut problems,
    );
    let repeat = configured_key(settings.repeat_key(), keys::REPEAT, "repeat", &mut problems);
    let reserved: std::collections::HashSet<_> = [(screen.0, screen.1), (repeat.0, repeat.1)]
        .into_iter()
        .chain(
            settings
                .presets
                .iter()
                .filter_map(|preset| preset.hotkey.as_deref().and_then(keys::parse)),
        )
        .collect();
    let capture_key = if register_hotkey(hwnd, HOTKEY_CAPTURE, primary.0, primary.1) {
        primary.2.clone()
    } else if let Some(&(_, _, name)) = keys::CAPTURE_FALLBACKS.iter().find(|(modifiers, vk, _)| {
        !reserved.contains(&(*modifiers, *vk))
            && register_hotkey(hwnd, HOTKEY_CAPTURE, *modifiers, *vk)
    }) {
        problems.push(format!(
            "{} is unavailable because {}; {} captures until it frees up",
            keys::display(&primary.2),
            hotkey_owner(&primary.2),
            keys::display(name)
        ));
        name.to_owned()
    } else {
        problems.push(format!(
            "{} is unavailable because {} and no fallback key is free; use the tray icon",
            keys::display(&primary.2),
            hotkey_owner(&primary.2)
        ));
        "the tray icon".to_owned()
    };
    if !register_hotkey(hwnd, HOTKEY_SCREEN, screen.0, screen.1) {
        problems.push(format!(
            "{} (full screen) is taken",
            keys::display(&screen.2)
        ));
    }
    if !register_hotkey(hwnd, HOTKEY_REPEAT, repeat.0, repeat.1) {
        problems.push(format!(
            "{} (repeat last region) is taken",
            keys::display(&repeat.2)
        ));
    }
    (capture_key, problems)
}

/// Called periodically while a fallback is in use, to reclaim the configured key.
fn retry_capture_key(hwnd: HWND) {
    let Some(settings) = with_app(|app| app.settings.clone()) else {
        return;
    };
    let primary = configured_key(
        settings.capture_key(),
        keys::CAPTURE,
        "capture",
        &mut Vec::new(),
    );
    if with_app(|app| app.capture_key == primary.2).unwrap_or(true) {
        // SAFETY: a timer on our own window.
        unsafe { KillTimer(hwnd, TIMER_HOTKEY_RETRY) };
        return;
    }
    // SAFETY: re-registers our own hotkey id.
    unsafe { UnregisterHotKey(hwnd, HOTKEY_CAPTURE) };
    if register_hotkey(hwnd, HOTKEY_CAPTURE, primary.0, primary.1) {
        with_app(|app| app.capture_key = primary.2.clone());
        log(&format!(
            "{0} became free; capture key is now {0}",
            primary.2
        ));
        publish_state();
        notify(
            hwnd,
            "Jiffshot hotkeys",
            &format!(
                "{} now starts a Jiffshot capture",
                keys::display(&primary.2)
            ),
            false,
        );
        // SAFETY: as above.
        unsafe { KillTimer(hwnd, TIMER_HOTKEY_RETRY) };
    } else {
        let key = with_app(|app| app.capture_key.clone()).unwrap_or_default();
        if let Some(&(modifiers, vk, _)) = keys::CAPTURE_FALLBACKS
            .iter()
            .find(|(_, _, name)| *name == key)
        {
            if !register_hotkey(hwnd, HOTKEY_CAPTURE, modifiers, vk) {
                notify(
                    hwnd,
                    "Jiffshot hotkeys",
                    &format!(
                        "{} is no longer available; use the tray icon",
                        keys::display(&key)
                    ),
                    true,
                );
                with_app(|app| app.capture_key = "the tray icon".to_owned());
                publish_state();
            }
        }
    }
}

#[cfg(windows)]
fn cursor_pos() -> (i32, i32) {
    sys::cursor_pos()
}

fn spawn_finish(capture: Capture, settings: Settings, owner: HWND) {
    let owner = owner as isize;
    #[cfg(target_os = "macos")]
    spawn_finish_after(capture, settings, owner, || Ok(()));
    #[cfg(not(target_os = "macos"))]
    std::thread::spawn(move || {
        let outcome = workflow::finish(capture, &settings, owner);
        // SAFETY: the window outlives worker threads in practice; if it is
        // gone the post fails and the box is leaked, which is harmless at exit.
        unsafe {
            PostMessageW(
                owner as HWND,
                WM_APP_DONE,
                0,
                Box::into_raw(Box::new(outcome)) as LPARAM,
            )
        };
    });
}

#[cfg(target_os = "macos")]
fn spawn_finish_after(
    capture: Capture,
    settings: Settings,
    owner: isize,
    before: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    crate::shutdown_macos::spawn(move || {
        if let Err(error) = before() {
            notify(owner as HWND, "Capture finalization failed", &error, true);
            return Some(error);
        }
        report_finished_capture(owner as HWND, workflow::finish(capture, &settings, owner))
    });
}

#[cfg(target_os = "macos")]
pub(crate) fn start_image_shutdown_selftest(
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    release: std::sync::mpsc::Receiver<()>,
) {
    let Some(owner) = with_app(|app| app.hwnd as isize) else {
        let _ = ready.send(Err("the capture service is unavailable".into()));
        return;
    };
    let mut frame = jiffshot_capture::Frame::new(16, 12);
    for pixel in frame.pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[14, 38, 207, 255]);
    }
    let capture = workflow::from_frame(CaptureKind::Region, Rect::new(0, 0, 16, 12), frame);
    let settings = Settings {
        open_in_editor: false,
        copy_to_clipboard: false,
        share: "none".into(),
        ..Default::default()
    };
    spawn_finish_after(capture, settings, owner, move || {
        ready
            .send(Ok(()))
            .map_err(|_| "the image shutdown fixture lost its ready receiver".to_owned())?;
        release
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|error| format!("the image shutdown fixture was not released: {error}"))
    });
}

#[cfg(target_os = "macos")]
fn report_finished_capture(owner: HWND, outcome: Outcome) -> Option<String> {
    if outcome.is_error || !outcome.opened_editor {
        let title = if outcome.is_error {
            "Capture saved with problems"
        } else {
            "Capture saved"
        };
        notify(owner, title, &outcome.message, outcome.is_error);
    }
    outcome.is_error.then_some(outcome.message)
}

#[cfg(target_os = "macos")]
struct ReopenCaptureWindow;

#[cfg(target_os = "macos")]
impl Drop for ReopenCaptureWindow {
    fn drop(&mut self) {
        crate::show_capture_window();
    }
}

fn select_create_mode(choice: Option<usize>) -> Option<usize> {
    const MODES: [usize; 4] = [MODE_IMAGE, MODE_VIDEO, MODE_SCROLLING, MODE_PANORAMIC];
    choice.and_then(|index| MODES.get(index).copied())
}

/// Freezes the screen and runs the selection overlay; `mode` says what to
/// do with the selection.
fn capture_interactive(mode: usize) {
    #[cfg(target_os = "macos")]
    let _reopen = (mode == MODE_SCHEDULE_AREA).then_some(ReopenCaptureWindow);
    let Some((hwnd, settings)) = with_app(|app| {
        if app.busy {
            return None;
        }
        app.busy = true;
        Some((app.hwnd, app.settings.clone()))
    })
    .flatten() else {
        return;
    };

    let started = Instant::now();
    #[cfg(target_os = "macos")]
    let display_layout = match screen::DisplayLayout::current() {
        Ok(layout) => layout,
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let capture_layout = CaptureLayout::from_display(display_layout.clone());
    #[cfg(windows)]
    let capture_layout = match CaptureLayout::current() {
        Ok(layout) => layout,
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    #[cfg(windows)]
    let cursor = CursorSnapshot::take();
    #[cfg(target_os = "macos")]
    let cursor = CursorSnapshot::take_in_layout(&display_layout);
    #[cfg(windows)]
    let pointer = cursor_pos();
    #[cfg(target_os = "macos")]
    let pointer = screen::try_cursor_pos_points().and_then(|(x, y)| {
        let precise = (
            x * display_layout.desktop_scale(),
            y * display_layout.desktop_scale(),
        );
        Ok((overlay::rounded_desktop_point(precise)?, precise))
    });
    #[cfg(windows)]
    let desktop = screen::virtual_screen();
    #[cfg(target_os = "macos")]
    let desktop = display_layout.virtual_screen();
    #[cfg(windows)]
    let snapshot = screen::grab(desktop).map(|frame| (frame, ()));
    #[cfg(target_os = "macos")]
    let snapshot = display_layout
        .freeze()
        .map(|frozen| (jiffshot_capture::Frame::new(0, 0), frozen));
    let prepared = snapshot.and_then(|(snapshot, frozen)| {
        #[cfg(windows)]
        let _ = frozen;
        let grabbed = started.elapsed();
        #[cfg(windows)]
        let targets = Targets::enumerate(desktop, &[hwnd as isize]);
        #[cfg(target_os = "macos")]
        let targets = Targets::enumerate_in_layout(desktop, &[hwnd as isize], &display_layout)?;
        let enumerated = started.elapsed();
        #[cfg(windows)]
        let monitors = screen::monitors();
        #[cfg(target_os = "macos")]
        let monitors = {
            display_layout.validate()?;
            display_layout.monitors().to_vec()
        };
        #[cfg(target_os = "macos")]
        let (pointer, precise_pointer) = pointer?;
        #[cfg(target_os = "macos")]
        overlay::client_point(precise_pointer, (desktop.x, desktop.y))?;
        let overlay = Overlay::new(snapshot, (desktop.x, desktop.y), targets, monitors, pointer)?;
        #[cfg(target_os = "macos")]
        let overlay = {
            let mut overlay = overlay
                .with_display_layout(capture_layout.clone())
                .with_frozen(frozen, desktop);
            overlay.remember_pointer_position(precise_pointer);
            overlay
        };
        log(&format!(
            "overlay ready in {:.0} ms (grab {:.0}, windows {:.0}, setup {:.0}) for {}x{}",
            started.elapsed().as_secs_f64() * 1000.0,
            grabbed.as_secs_f64() * 1000.0,
            (enumerated - grabbed).as_secs_f64() * 1000.0,
            (started.elapsed() - enumerated).as_secs_f64() * 1000.0,
            desktop.width,
            desktop.height
        ));
        Ok(overlay)
    });
    let overlay = match prepared {
        Ok(overlay) => overlay,
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };

    let mut overlay = overlay;
    overlay.started = Some(started);
    overlay.mode = match mode {
        MODE_FREEHAND => overlay::Mode::Freehand,
        MODE_MULTI => overlay::Mode::MultiArea,
        MODE_FIXED => {
            overlay::Mode::Fixed(settings.fixed_size.0.max(16), settings.fixed_size.1.max(16))
        }
        MODE_VIDEO => overlay::Mode::Video,
        _ => overlay::Mode::Normal,
    };
    let (overlay, selection) = match overlay::run(overlay) {
        Ok(result) => result,
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    // For checking the overlay live: the selection is logged, nothing is
    // saved or copied.
    if std::env::var_os("JIFFSHOT_CAPTURE_DRY_RUN").is_some() {
        log(&format!("dry run: selected {selection:?}"));
        with_app(|app| app.busy = false);
        return;
    }
    #[cfg(target_os = "macos")]
    if mode == MODE_SCHEDULE_AREA {
        if let Some(selection) = selection {
            let (title, message, warning) = scheduled_selection_notice(save_scheduled_selection(
                hwnd,
                &selection,
                capture_layout,
            ));
            notify(hwnd, title, &message, warning);
        }
        with_app(|app| app.busy = false);
        return;
    }
    let mut settings = settings;
    if let Some((cursor, effects, share)) = with_app(|app| {
        (
            app.override_cursor.take(),
            app.override_effects.take(),
            app.override_share.take(),
        )
    }) {
        if let Some(c) = cursor {
            settings.include_cursor = c;
        }
        if let Some(to) = share {
            settings.share = to;
        }
        if let Some(e) = effects {
            settings.capture_effects = e;
        }
    }
    let Some(selection) = selection else {
        with_app(|app| app.busy = false);
        return;
    };
    let rect = selection.rect();
    #[cfg(target_os = "macos")]
    let capture_layout = match capture_layout.for_selection(&selection, &overlay.targets) {
        Ok(layout) => layout,
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    let include_cursor = settings.include_cursor
        && matches!(
            mode,
            MODE_ALL | MODE_IMAGE | MODE_WINDOW | MODE_FREEHAND | MODE_MULTI | MODE_FIXED
        );
    let capture = match workflow::from_overlay(
        &overlay,
        &selection,
        cursor.as_ref(),
        include_cursor,
        settings.cursor_as_object,
    ) {
        Ok(Some(capture)) => capture,
        Ok(None) => {
            with_app(|app| app.busy = false);
            notify(
                hwnd,
                "Capture failed",
                "The selected area is outside the frozen desktop.",
                true,
            );
            return;
        }
        Err(error) => {
            with_app(|app| app.busy = false);
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    drop(overlay);
    #[cfg(windows)]
    {
        with_app(|app| {
            app.last_region = Some(CaptureRegion {
                rect,
                layout: capture_layout.clone(),
            })
        });
        publish_state();
    }
    // Choose asks what to make of the area once it is selected.
    let mode = if mode == MODE_ALL {
        match select_create_mode(crate::hud::run_panel(
            rect,
            crate::panel::SELECT_CREATE_PROMPT,
            &crate::panel::SELECT_CREATE_CHOICES,
            1000,
            || None,
        )) {
            Some(mode) => mode,
            None => {
                with_app(|app| app.busy = false);
                return;
            }
        }
    } else {
        mode
    };
    if let Err(error) = capture_layout.validate() {
        with_app(|app| app.busy = false);
        notify(hwnd, "Capture failed", &error.to_string(), true);
        return;
    }
    #[cfg(target_os = "macos")]
    {
        with_app(|app| {
            app.last_region = Some(CaptureRegion {
                rect,
                layout: capture_layout.clone(),
            })
        });
        publish_state();
    }
    match mode {
        MODE_TEXT => match modes::text(&capture.frame) {
            Ok(text) if !text.trim().is_empty() => {
                let copied = jiffshot_capture::clipboard::copy_text(&text, hwnd as isize);
                let preview: String = text.chars().take(180).collect();
                #[cfg(windows)]
                let title = if copied.is_ok() {
                    "Text copied"
                } else {
                    "Text found (clipboard busy)"
                };
                log(&format!("grab text: {} characters", text.chars().count()));
                #[cfg(windows)]
                notify(hwnd, title, &preview, false);
                #[cfg(target_os = "macos")]
                match copied {
                    Ok(()) => notify(hwnd, "Text copied", &preview, false),
                    Err(error) => notify(
                        hwnd,
                        "Grab text",
                        &format!("Text was found but could not be copied: {error}"),
                        true,
                    ),
                }
                crate::show_text(&text);
            }
            Ok(_) => notify(hwnd, "Grab text", "No text found in the selection", true),
            Err(error) => notify(hwnd, "Grab text failed", &error, true),
        },
        // These run until done, so they stay busy until then.
        MODE_SCROLLING => {
            return spawn_scrolling(
                modes::scrolling_in_layout,
                rect,
                settings,
                hwnd,
                capture_layout,
            )
        }
        MODE_SCROLL_BOTH => {
            return spawn_scrolling(
                modes::scrolling_both_in_layout,
                rect,
                settings,
                hwnd,
                capture_layout,
            )
        }
        MODE_SCROLL_H => {
            return spawn_scrolling(
                modes::scrolling_horizontal_in_layout,
                rect,
                settings,
                hwnd,
                capture_layout,
            )
        }
        MODE_PANORAMIC => match modes::panoramic_in_layout(rect, &capture_layout) {
            Ok(Some(frame)) => spawn_finish(
                workflow::from_frame(CaptureKind::Panoramic, rect, frame),
                settings,
                hwnd,
            ),
            Ok(None) => {}
            Err(error) => notify(hwnd, "Panoramic capture failed", &error.to_string(), true),
        },
        MODE_VIDEO => match crate::recorder::record_in_layout(rect, &settings, capture_layout) {
            Ok(Some(path)) => spawn_video_finish(path, rect, settings, hwnd),
            Ok(None) => {}
            Err(error) => {
                #[cfg(target_os = "macos")]
                crate::shutdown_macos::work_failed(&error);
                notify(hwnd, "Recording failed", &error, true);
            }
        },
        _ => spawn_finish(capture, settings, hwnd),
    }
    with_app(|app| app.busy = false);
}

/// Runs a scrolling capture on a worker thread (it scrolls and grabs for a
/// while), then finishes it here; busy until then.
fn spawn_scrolling(
    scroll: fn(Rect, &CaptureLayout) -> std::io::Result<jiffshot_capture::Frame>,
    rect: Rect,
    settings: Settings,
    owner: HWND,
    layout: CaptureLayout,
) {
    let owner = owner as isize;
    #[cfg(target_os = "macos")]
    crate::shutdown_macos::spawn(move || {
        let started = Instant::now();
        let result = scroll(rect, &layout);
        if !crate::shutdown_macos::requested()
            && !sys::post_to_class(CLASS, WM_APP_SCROLL_DONE, 0, 0)
            && !crate::shutdown_macos::requested()
        {
            log("could not update the capture service after scrolling");
        }
        match result {
            Ok(frame) => {
                log(&format!(
                    "scrolling capture {}x{} in {:.1} s",
                    frame.width,
                    frame.height,
                    started.elapsed().as_secs_f64()
                ));
                report_finished_capture(
                    owner as HWND,
                    workflow::finish(
                        workflow::from_frame(CaptureKind::Scrolling, rect, frame),
                        &settings,
                        owner,
                    ),
                )
            }
            Err(error) => {
                let message = error.to_string();
                notify(owner as HWND, "Scrolling capture failed", &message, true);
                Some(format!("Scrolling capture failed: {message}"))
            }
        }
    });
    #[cfg(not(target_os = "macos"))]
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = scroll(rect, &layout);
        crate::on_capture_thread(move || {
            with_app(|app| app.busy = false);
            let hwnd = owner as HWND;
            match result {
                Ok(frame) => {
                    log(&format!(
                        "scrolling capture {}x{} in {:.1} s",
                        frame.width,
                        frame.height,
                        started.elapsed().as_secs_f64()
                    ));
                    spawn_finish(
                        workflow::from_frame(CaptureKind::Scrolling, rect, frame),
                        settings,
                        hwnd,
                    );
                }
                Err(error) => notify(hwnd, "Scrolling capture failed", &error.to_string(), true),
            }
        });
    });
}

/// Opens a finished recording like a capture (editor or notification).
fn spawn_video_finish(path: std::path::PathBuf, rect: Rect, settings: Settings, owner: HWND) {
    let owner = owner as isize;
    #[cfg(target_os = "macos")]
    crate::shutdown_macos::spawn(move || {
        report_finished_capture(
            owner as HWND,
            workflow::finish_video(&path, rect, &settings, owner),
        )
    });
    #[cfg(not(target_os = "macos"))]
    std::thread::spawn(move || {
        let outcome = workflow::finish_video(&path, rect, &settings, owner);
        // SAFETY: as in `spawn_finish`.
        unsafe {
            PostMessageW(
                owner as HWND,
                WM_APP_DONE,
                0,
                Box::into_raw(Box::new(outcome)) as LPARAM,
            )
        };
    });
}

fn finish_step_guide(
    doc: &jiffshot_doc::Document,
    path: &std::path::Path,
    count: usize,
    settings: &Settings,
) -> Outcome {
    let editable = path.with_extension(jiffshot_capture::library::EDITS_EXT);
    let outcome = match jiffshot_doc::io::save_document(doc, &editable) {
        Err(error) => Outcome {
            message: format!("The Step Capture image was saved to {}, but its editable guide could not be saved: {error}", path.display()),
            is_error: true,
            opened_editor: false,
        },
        Ok(()) => {
            let opened = settings.open_in_editor && workflow::open_editor(settings, path);
            Outcome { message: format!("Step Capture: {count} steps saved"), is_error: false, opened_editor: opened }
        }
    };
    #[cfg(target_os = "macos")]
    let outcome = {
        let mut outcome = outcome;
        if let Some(notice) =
            jiffshot_doc::text::describe_font_substitutions(&doc.font_substitutions())
        {
            outcome.message = format!("{notice}; {}", outcome.message);
        }
        outcome
    };
    outcome
}

/// Records a Step Capture and opens the guide it makes.
fn capture_steps() {
    let Some((hwnd, settings)) = with_app(|app| {
        if app.busy {
            return None;
        }
        app.busy = true;
        Some((app.hwnd, app.settings.clone()))
    })
    .flatten() else {
        return;
    };
    let steps = crate::stepcap::record();
    with_app(|app| app.busy = false);
    let steps = match steps {
        Ok(Some(steps)) => steps,
        Ok(None) => return,
        Err(error) => {
            #[cfg(target_os = "macos")]
            crate::shutdown_macos::work_failed(&format!("Step Capture failed: {error}"));
            notify(hwnd, "Step Capture failed", &error.to_string(), true);
            return;
        }
    };
    let doc = match crate::stepcap::guide(&steps) {
        Ok(doc) => doc,
        Err(error) => {
            #[cfg(target_os = "macos")]
            crate::shutdown_macos::work_failed(&format!("Step Capture failed: {error}"));
            notify(hwnd, "Step Capture failed", &error.to_string(), true);
            return;
        }
    };
    let owner = hwnd as isize;
    let finish = move || {
        let library = Library::new(jiffshot_capture::config::library_root());
        let frame = workflow::frame_from_pixmap(&doc.flatten());
        let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
        let capture = workflow::from_frame(CaptureKind::Steps, rect, frame);
        let outcome = match library.save(&capture.frame, &capture.meta) {
            Ok(path) => finish_step_guide(&doc, &path, steps.len(), &settings),
            Err(error) => Outcome {
                message: format!("Could not save the guide: {error}"),
                is_error: true,
                opened_editor: false,
            },
        };
        outcome
    };
    #[cfg(target_os = "macos")]
    crate::shutdown_macos::spawn(move || report_finished_capture(owner as HWND, finish()));
    #[cfg(not(target_os = "macos"))]
    std::thread::spawn(move || {
        // SAFETY: as in `spawn_finish`.
        unsafe {
            PostMessageW(
                owner as HWND,
                WM_APP_DONE,
                0,
                Box::into_raw(Box::new(finish())) as LPARAM,
            )
        };
    });
}

/// Takes the image on the clipboard (or an image file copied there) as a
/// capture.
fn capture_from_clipboard() {
    let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) else {
        return;
    };
    #[cfg(target_os = "macos")]
    let frame = jiffshot_capture::clipboard::read(hwnd as isize)
        .map_err(|error| format!("Could not read the clipboard: {error}"))
        .and_then(frame_from_clipboard);
    #[cfg(windows)]
    let frame = match jiffshot_capture::clipboard::read(hwnd as isize) {
        Ok(Some(jiffshot_capture::clipboard::Pasted::Image {
            width,
            height,
            rgba,
        })) => {
            let mut f = jiffshot_capture::Frame::new(width, height);
            f.alpha = rgba.chunks_exact(4).any(|p| p[3] < 255);
            for (d, s) in f.pixels.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
            Some(f)
        }
        Ok(Some(jiffshot_capture::clipboard::Pasted::Files(files))) => files
            .iter()
            .find_map(|p| jiffshot_doc::io::load_image(p).ok())
            .map(|pm| workflow::frame_from_pixmap(&pm)),
        _ => None,
    };
    #[cfg(windows)]
    let frame: Result<Option<jiffshot_capture::Frame>, String> = Ok(frame);
    match frame {
        Ok(Some(frame)) => {
            let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
            spawn_finish(
                workflow::from_frame(CaptureKind::Region, rect, frame),
                settings,
                hwnd,
            );
        }
        Ok(None) => notify(
            hwnd,
            "Capture from clipboard",
            "There is no image on the clipboard",
            true,
        ),
        Err(error) => notify(hwnd, "Capture from clipboard", &error, true),
    }
}

#[cfg(target_os = "macos")]
fn frame_from_clipboard(
    pasted: Option<jiffshot_capture::clipboard::Pasted>,
) -> Result<Option<jiffshot_capture::Frame>, String> {
    use jiffshot_capture::clipboard::Pasted;
    match pasted {
        Some(Pasted::Image {
            width,
            height,
            rgba,
        }) => {
            if width > i32::MAX as u32 || height > i32::MAX as u32 {
                return Err("The clipboard image dimensions are too large".into());
            }
            let alpha = rgba.chunks_exact(4).any(|pixel| pixel[3] < 255);
            let mut frame = jiffshot_capture::Frame {
                width,
                height,
                pixels: rgba,
                alpha,
            };
            frame
                .validate()
                .map_err(|error| format!("Invalid clipboard image: {error}"))?;
            for pixel in frame.pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            Ok(Some(frame))
        }
        Some(Pasted::Files(files)) => {
            let mut errors = Vec::new();
            for path in files {
                match jiffshot_doc::io::load_image(&path) {
                    Ok(image) => return Ok(Some(workflow::frame_from_pixmap(&image))),
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
            if errors.is_empty() {
                Ok(None)
            } else {
                Err(format!(
                    "Could not open the clipboard image file(s): {}",
                    errors.join("; ")
                ))
            }
        }
        Some(Pasted::Text(_)) | None => Ok(None),
    }
}

/// Asks for image files (Windows' Open dialog).
#[cfg(windows)]
fn choose_images(hwnd: HWND) -> Vec<std::path::PathBuf> {
    use jiffshot_capture::wide;
    use std::path::PathBuf;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OPENFILENAMEW,
    };
    let mut buf = vec![0u16; 32 * 1024];
    let filter: Vec<u16> = "Images\0*.png;*.jpg;*.jpeg;*.bmp;*.gif;*.webp\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide("Capture from file");
    // SAFETY: a plain C struct; zero is valid for every field.
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_ALLOWMULTISELECT;
    // SAFETY: every pointer in `ofn` outlives the modal call.
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return Vec::new();
    }
    // One file: its full path. Several: the folder, then each name.
    let parts: Vec<String> = buf
        .split(|&c| c == 0)
        .take_while(|p| !p.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.as_slice() {
        [one] => vec![PathBuf::from(one)],
        [dir, names @ ..] => names.iter().map(|n| PathBuf::from(dir).join(n)).collect(),
        [] => Vec::new(),
    }
}

/// Asks for image files (the standard Open dialog).
#[cfg(target_os = "macos")]
fn choose_images() -> std::io::Result<Vec<std::path::PathBuf>> {
    jiffshot_capture::os::choose_open_paths(
        "Capture from file",
        &["png", "jpg", "jpeg", "bmp", "gif", "webp"],
        None,
    )
}

/// Asks for image files and adds each to the library as a capture.
fn capture_from_file() {
    let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) else {
        return;
    };
    #[cfg(windows)]
    let files = choose_images(hwnd);
    #[cfg(windows)]
    import_files(files, settings, hwnd);

    #[cfg(target_os = "macos")]
    {
        let owner = hwnd as isize;
        crate::shutdown_macos::spawn(move || {
            let hwnd = owner as HWND;
            match choose_images() {
                Ok(files) => import_files(files, settings, hwnd),
                Err(error) => {
                    let message = error.to_string();
                    notify(hwnd, "Capture from file", &message, true);
                    Some(format!("Capture from file failed: {message}"))
                }
            }
        });
    }
}

fn import_files(files: Vec<std::path::PathBuf>, settings: Settings, hwnd: HWND) -> Option<String> {
    #[cfg(target_os = "macos")]
    let mut errors = Vec::new();
    for (i, path) in files.iter().enumerate() {
        match jiffshot_doc::io::load_image(path) {
            Ok(pm) => {
                let frame = workflow::frame_from_pixmap(&pm);
                let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
                let mut s = settings.clone();
                // Only the last one opens in the editor.
                s.open_in_editor &= i + 1 == files.len();
                s.copy_to_clipboard = false;
                let capture = workflow::from_frame(CaptureKind::Region, rect, frame);
                #[cfg(target_os = "macos")]
                if let Some(error) =
                    report_finished_capture(hwnd, workflow::finish(capture, &s, hwnd as isize))
                {
                    errors.push(error);
                }
                #[cfg(windows)]
                spawn_finish(capture, s, hwnd);
            }
            Err(error) => {
                let message = format!("{}: {error}", path.display());
                notify(hwnd, "Capture from file", &message, true);
                #[cfg(target_os = "macos")]
                errors.push(message);
            }
        }
    }
    #[cfg(target_os = "macos")]
    return (!errors.is_empty()).then(|| errors.join("; "));
    #[cfg(windows)]
    None
}

/// Minutes from now until the next `HH:MM` (local time).
pub(crate) fn minutes_until(at: &str) -> Option<u32> {
    let (h, m) = at.trim().split_once(':')?;
    let target = h.parse::<u32>().ok()?.min(23) * 60 + m.parse::<u32>().ok()?.min(59);
    let (_, _, _, hour, minute, _) = jiffshot_capture::os::local_time();
    let now = hour as u32 * 60 + minute as u32;
    Some((target + 24 * 60 - now) % (24 * 60))
}

/// Arms (or disarms) the scheduled capture from `settings.schedule`.
fn arm_schedule(hwnd: HWND, schedule: Option<&str>) -> std::io::Result<()> {
    // SAFETY: timers on our own window.
    unsafe {
        KillTimer(hwnd, TIMER_SCHEDULE);
        if schedule.and_then(minutes_until).is_some()
            && SetTimer(hwnd, TIMER_SCHEDULE, 20_000, None) == 0
        {
            return Err(std::io::Error::other(
                "could not start the scheduled capture timer",
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn restore_saved_coordinates(settings: &Settings) {
    let fixed = settings
        .capture_window
        .fixed_at
        .map(|(x, y)| Rect::new(x, y, settings.fixed_size.0, settings.fixed_size.1));
    let scheduled = settings
        .schedule_area
        .map(|(x, y, width, height)| Rect::new(x, y, width, height));
    for (kind, rect, confirmation) in [
        (
            SavedAreaKind::Fixed,
            fixed,
            settings.macos_fixed_area.as_ref(),
        ),
        (
            SavedAreaKind::Scheduled,
            scheduled,
            settings.macos_schedule_area.as_ref(),
        ),
    ] {
        let result = match (rect, confirmation) {
            (Some(rect), Some(confirmation)) => {
                geometry::restore_saved_area(kind, rect, confirmation)
            }
            _ => geometry::clear_saved_area(kind),
        };
        if let Err(error) = result {
            log(&format!("saved capture confirmation needs review: {error}"));
        }
    }
}

#[cfg(target_os = "macos")]
fn check_scheduled_area(settings: &Settings) -> std::io::Result<()> {
    if let Some((x, y, width, height)) = settings.schedule_area {
        saved_area_layout(SavedAreaKind::Scheduled, Rect::new(x, y, width, height))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct SavedScheduledSelection {
    session_only: bool,
    cancelled_pending: bool,
    paused: Option<std::io::Error>,
}

#[cfg(target_os = "macos")]
fn scheduled_selection_notice(
    result: std::io::Result<SavedScheduledSelection>,
) -> (&'static str, String, bool) {
    match result {
        Ok(saved) => {
            let (title, mut message) = match saved.paused {
                Some(ref error) => ("Scheduled area saved; capture paused", error.to_string()),
                None => (
                    "Scheduled area saved",
                    "The selected area is ready for scheduled capture.".to_owned(),
                ),
            };
            if saved.cancelled_pending {
                message.push_str(" A scheduled capture waiting for the previous area was skipped.");
            }
            if saved.session_only {
                message.push(' ');
                message.push_str(geometry::SESSION_ONLY_AREA_WARNING);
            }
            (
                title,
                message,
                saved.paused.is_some() || saved.cancelled_pending || saved.session_only,
            )
        }
        Err(error) => ("Scheduled area not saved", error.to_string(), true),
    }
}

#[cfg(target_os = "macos")]
fn save_scheduled_selection(
    hwnd: HWND,
    selection: &overlay::Selection,
    layout: CaptureLayout,
) -> std::io::Result<SavedScheduledSelection> {
    save_scheduled_selection_with(hwnd, selection, layout, Settings::try_save)
}

#[cfg(target_os = "macos")]
fn save_scheduled_selection_with(
    hwnd: HWND,
    selection: &overlay::Selection,
    layout: CaptureLayout,
    save: impl FnOnce(&Settings) -> std::io::Result<()>,
) -> std::io::Result<SavedScheduledSelection> {
    let mut settings = with_app(|app| app.settings.clone())
        .ok_or_else(|| std::io::Error::other("the capture service is unavailable"))?;
    let rect = selection.rect();
    let confirmation = geometry::confirm_saved_area_in_layout_after(
        SavedAreaKind::Scheduled,
        rect,
        layout,
        |confirmation| {
            settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));
            settings.macos_schedule_area = Some(confirmation.clone());
            save(&settings).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not save the selected area: {error}"),
                )
            })
        },
    )?;
    let cancelled_pending = with_app(|app| {
        app.settings = settings.clone();
        app.deferred_schedule.take().is_some()
    })
    .ok_or_else(|| std::io::Error::other("the capture service is unavailable"))?;
    let validation = saved_area_ticket(SavedAreaKind::Scheduled, rect).err();
    let scheduling = synchronize_schedule(hwnd, &settings).err();
    publish_state();
    Ok(SavedScheduledSelection {
        session_only: confirmation.session_only,
        cancelled_pending,
        paused: validation.or(scheduling),
    })
}

#[cfg(target_os = "macos")]
fn synchronize_schedule(hwnd: HWND, settings: &Settings) -> std::io::Result<()> {
    let validation = match settings.schedule.as_deref() {
        Some(at) if minutes_until(at).is_none() => Err(std::io::Error::other(
            "Review the scheduled time in Capture > Time delay.",
        )),
        Some(_) => check_scheduled_area(settings),
        None => Ok(()),
    };
    let should_arm = settings.schedule.is_some() && validation.is_ok();
    let was_armed = with_app(|app| app.schedule_armed).unwrap_or(false);
    if should_arm != was_armed {
        arm_schedule(
            hwnd,
            if should_arm {
                settings.schedule.as_deref()
            } else {
                None
            },
        )?;
        with_app(|app| app.schedule_armed = should_arm);
    }
    validation
}

fn schedule_tick() {
    let Some((hwnd, settings, last)) =
        with_app(|app| (app.hwnd, app.settings.clone(), app.scheduled_fired))
    else {
        return;
    };
    let Some(at) = settings.schedule.as_deref() else {
        return;
    };
    #[cfg(target_os = "macos")]
    if let Err(error) = synchronize_schedule(hwnd, &settings) {
        with_app(|app| app.deferred_schedule = None);
        notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
        publish_state();
        return;
    }
    let due = minutes_until(at) == Some(0)
        && !last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(120));
    #[cfg(target_os = "macos")]
    if defer_busy_schedule(hwnd, &settings, due) {
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(pending) = with_app(|app| app.deferred_schedule.take()).flatten() {
        match pending.validate(&settings, std::time::Instant::now()) {
            Ok(()) => capture_scheduled_checked(hwnd, settings, pending.ticket),
            Err(error) => notify(hwnd, "Scheduled capture skipped", &error.to_string(), true),
        }
        return;
    }
    if !due {
        return;
    }
    capture_scheduled(hwnd, settings);
}

#[cfg(target_os = "macos")]
fn defer_busy_schedule(hwnd: HWND, settings: &Settings, due: bool) -> bool {
    if with_app(|app| app.busy).unwrap_or(true) {
        let expired = with_app(|app| {
            if app.deferred_schedule.as_ref().is_some_and(|pending| {
                pending.queued.elapsed() > DeferredScheduledCapture::MAX_WAIT
            }) {
                app.deferred_schedule.take()
            } else {
                None
            }
        })
        .flatten();
        if expired.is_some() {
            notify(
                hwnd,
                "Scheduled capture skipped",
                "Another capture kept it waiting for more than two minutes.",
                true,
            );
        }
        if due && with_app(|app| app.deferred_schedule.is_none()).unwrap_or(false) {
            let now = std::time::Instant::now();
            match DeferredScheduledCapture::new(&settings, now) {
                Ok(pending) => {
                    with_app(|app| {
                        app.scheduled_fired = Some(now);
                        app.deferred_schedule = Some(pending);
                    });
                    notify(
                        hwnd,
                        "Scheduled capture delayed",
                        "It will run after the current capture finishes, for up to two minutes.",
                        false,
                    );
                }
                Err(error) => {
                    unsafe { KillTimer(hwnd, TIMER_SCHEDULE) };
                    with_app(|app| app.schedule_armed = false);
                    notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
                    publish_state();
                }
            }
        }
        true
    } else {
        false
    }
}

fn capture_scheduled(hwnd: HWND, settings: Settings) {
    #[cfg(target_os = "macos")]
    let scheduled_ticket = match settings
        .schedule_area
        .map(|(x, y, width, height)| {
            saved_area_ticket(SavedAreaKind::Scheduled, Rect::new(x, y, width, height))
        })
        .transpose()
    {
        Ok(ticket) => ticket,
        Err(error) => {
            unsafe { KillTimer(hwnd, TIMER_SCHEDULE) };
            with_app(|app| app.schedule_armed = false);
            notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
            publish_state();
            return;
        }
    };
    #[cfg(target_os = "macos")]
    capture_scheduled_checked(hwnd, settings, scheduled_ticket);
    #[cfg(windows)]
    capture_scheduled_checked(hwnd, settings);
}

fn capture_scheduled_checked(
    hwnd: HWND,
    settings: Settings,
    #[cfg(target_os = "macos")] scheduled_ticket: Option<SavedAreaTicket>,
) {
    with_app(|app| app.scheduled_fired = Some(std::time::Instant::now()));
    #[cfg(target_os = "macos")]
    let capture = scheduled_capture(&settings, scheduled_ticket.as_ref());
    #[cfg(windows)]
    let capture = scheduled_capture(&settings);
    match capture {
        Ok(capture) => {
            let mut quiet = settings;
            quiet.open_in_editor = false;
            quiet.copy_to_clipboard = false;
            spawn_finish(capture, quiet, hwnd);
        }
        Err(error) => {
            #[cfg(target_os = "macos")]
            {
                if check_scheduled_area(&settings).is_err() {
                    unsafe { KillTimer(hwnd, TIMER_SCHEDULE) };
                    with_app(|app| app.schedule_armed = false);
                    publish_state();
                }
                notify(hwnd, "Scheduled capture failed", &error.to_string(), true);
            }
            #[cfg(windows)]
            let _ = error;
        }
    }
}

pub(crate) fn scheduled_capture(
    settings: &Settings,
    #[cfg(target_os = "macos")] scheduled_ticket: Option<&SavedAreaTicket>,
) -> std::io::Result<workflow::Capture> {
    let layout = CaptureLayout::current()?;
    let (rect, layout) = if let Some((x, y, w, h)) = settings.schedule_area {
        #[cfg(target_os = "macos")]
        let layout = {
            let ticket = scheduled_ticket.ok_or_else(|| {
                std::io::Error::other("the scheduled capture confirmation is missing")
            })?;
            if ticket.rect() != Rect::new(x, y, w, h) {
                return Err(std::io::Error::other(
                    "the scheduled capture area changed while the capture was waiting",
                ));
            }
            ticket.validate()?;
            ticket.layout().clone()
        };
        (Rect::new(x, y, w, h), layout)
    } else if let Some(monitor) = layout.monitor_under_cursor()? {
        (monitor.rect, layout.with_monitor(&monitor)?)
    } else {
        (layout.virtual_screen(), layout)
    };
    workflow::immediate_in_layout(CaptureKind::Region, rect, false, &layout)
}

fn arm_interval_timer(hwnd: HWND, seconds: u32) -> bool {
    // SAFETY: the timer belongs to our capture-thread window.
    unsafe { SetTimer(hwnd, TIMER_TIMELAPSE, seconds.max(1).saturating_mul(1000), None) != 0 }
}

fn rearm_interval_timer(hwnd: HWND, seconds: u32) -> Result<(), String> {
    if arm_interval_timer(hwnd, seconds) {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // SAFETY: the timer belongs to our capture-thread window.
    unsafe { KillTimer(hwnd, TIMER_TIMELAPSE) };
    with_app(|app| app.timelapse = None);
    Err(format!("Could not update the interval; capture stopped: {error}"))
}

/// Starts or stops the time-lapse (an interval capture of the last area,
/// or the screen under the pointer).
pub fn toggle_timelapse() {
    let Some((hwnd, running, seconds, last)) = with_app(|app| {
        (
            app.hwnd,
            app.timelapse.is_some(),
            app.settings.interval_seconds.max(1),
            app.last_region.clone(),
        )
    }) else {
        return;
    };
    if running {
        with_app(|app| app.timelapse = None);
        // SAFETY: a timer on our own window.
        unsafe { KillTimer(hwnd, TIMER_TIMELAPSE) };
        notify(hwnd, "Time-lapse", "Stopped", false);
        publish_state();
        return;
    }
    let region = match last {
        Some(region) => region,
        None => {
            let layout = match CaptureLayout::current() {
                Ok(layout) => layout,
                Err(error) => {
                    notify(hwnd, "Time-lapse failed", &error.to_string(), true);
                    return;
                }
            };
            let monitor = match layout.monitor_under_cursor() {
                Ok(monitor) => monitor,
                Err(error) => {
                    notify(hwnd, "Time-lapse failed", &error.to_string(), true);
                    return;
                }
            };
            let (rect, layout) = if let Some(monitor) = monitor {
                let selected = match layout.with_monitor(&monitor) {
                    Ok(layout) => layout,
                    Err(error) => {
                        notify(hwnd, "Time-lapse failed", &error.to_string(), true);
                        return;
                    }
                };
                (monitor.rect, selected)
            } else {
                (layout.virtual_screen(), layout)
            };
            CaptureRegion { rect, layout }
        }
    };
    if let Err(error) = region.layout.validate() {
        with_app(|app| app.last_region = None);
        notify(hwnd, "Time-lapse failed", &error.to_string(), true);
        publish_state();
        return;
    }
    if !arm_interval_timer(hwnd, seconds) {
        notify(hwnd, "Time-lapse", &format!("Could not start interval capture: {}", std::io::Error::last_os_error()), true);
        return;
    }
    with_app(|app| {
        app.timelapse = Some(TimeLapse {
            region,
            last_hash: 0,
        })
    });
    notify(
        hwnd,
        "Time-lapse",
        &format!("Capturing every {seconds} s. Stop it in Choose or from the tray menu."),
        false,
    );
    timelapse_tick();
    publish_state();
}

pub(crate) fn set_timelapse(action: crate::capture::IntervalAction) {
    if let Some(true) = with_app(|app| action.should_toggle(app.timelapse.is_some())) {
        toggle_timelapse();
    }
}

/// Stops interval capture if it is still running.
pub fn stop_timelapse() {
    set_timelapse(crate::capture::IntervalAction::Stop);
}

fn timelapse_tick() {
    let Some((hwnd, settings, Some(timelapse))) =
        with_app(|app| (app.hwnd, app.settings.clone(), app.timelapse.clone()))
    else {
        return;
    };
    let capture = match workflow::immediate_in_layout(
        CaptureKind::Region,
        timelapse.region.rect,
        false,
        &timelapse.region.layout,
    ) {
        Ok(capture) => capture,
        Err(error) => {
            #[cfg(target_os = "macos")]
            {
                with_app(|app| {
                    app.timelapse = None;
                    if timelapse.region.layout.validate().is_err() {
                        app.last_region = None;
                    }
                });
                unsafe { KillTimer(hwnd, TIMER_TIMELAPSE) };
                notify(hwnd, "Time-lapse failed", &error.to_string(), true);
                publish_state();
            }
            #[cfg(windows)]
            let _ = error;
            return;
        }
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in capture.frame.pixels.iter().step_by(7) {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    if h == timelapse.last_hash && settings.interval_skip_identical {
        return;
    }
    with_app(|app| {
        if let Some(timelapse) = app.timelapse.as_mut() {
            timelapse.last_hash = h;
        }
    });
    let mut quiet = settings;
    quiet.open_in_editor = false;
    quiet.copy_to_clipboard = false;
    spawn_finish(capture, quiet, hwnd);
}

// ---- for the Capture window ---------------------------------------------

/// Shows or hides the Capture Widget, and remembers it.
pub fn set_widget(on: bool) {
    with_app(|app| {
        app.settings.show_widget = on;
        app.settings.save();
    });
    // The app shows or hides the widget from the published state.
    publish_state();
}

/// Starts a capture with the Capture window's settings (already hidden):
/// its tab's mode, or the interval or schedule its Time Delay is set to.
pub fn start_from_panel(s: Settings) {
    let tab = s.capture_window.tab.min(2);
    apply_settings(s.clone());
    if tab != 2 && s.capture_window.timer {
        match s.capture_window.timing.as_str() {
            "interval" => return toggle_timelapse(),
            "schedule" => {
                let action = if s.schedule.is_some() {
                    crate::capture::ScheduleAction::Stop
                } else {
                    crate::capture::ScheduleAction::Start
                };
                return set_schedule_action(action);
            }
            _ => {}
        }
    }
    let mode = crate::panel::mode_of(&s, tab);
    match (mode.as_str(), s.capture_window.fixed_at) {
        ("fixed", Some((x, y))) if tab == 1 => {
            capture_rect_after_delay(Rect::new(x, y, s.fixed_size.0, s.fixed_size.1))
        }
        _ => start_mode_after_delay(&mode),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn start_from_current_panel(tab: usize) {
    let Some(mut settings) = with_app(|app| app.settings.clone()) else {
        log("Capture not started: the capture service is unavailable");
        return;
    };
    settings.capture_window.tab = tab.min(2);
    start_from_panel(settings);
}

#[cfg(target_os = "macos")]
fn commit_window_settings_with(
    settings: Settings,
    changes: geometry::SavedAreaChanges,
    save: impl FnOnce(&Settings) -> std::io::Result<()>,
) -> Result<Vec<String>, String> {
    let (hwnd, previous_schedule, rearm) = with_app(|app| {
        (
            app.hwnd,
            (
                app.settings.schedule.clone(),
                app.settings.schedule_area,
                app.schedule_armed,
            ),
            interval_to_rearm(app.timelapse.is_some(), app.settings.interval_seconds, settings.interval_seconds),
        )
    })
    .ok_or_else(|| "The capture service is unavailable.".to_owned())?;
    let changes_areas = !changes.is_empty();
    let mut warnings = match changes.commit_after(|| save(&settings)) {
        Ok(()) => Vec::new(),
        Err(error) if !changes_areas => vec![format!(
            "Capture settings were not saved and apply only until Jiffshot quits: {error}"
        )],
        Err(error) => return Err(format!("Could not save Capture settings: {error}")),
    };
    let cancelled_pending = with_app(|app| {
        let cancelled = app.deferred_schedule.as_ref().is_some_and(|pending| {
            settings.schedule.as_deref() != Some(pending.at.as_str())
                || settings.schedule_area != pending.area
        });
        if cancelled {
            app.deferred_schedule = None;
        }
        app.settings = settings.clone();
        cancelled && app.settings.schedule.is_some()
    })
    .ok_or_else(|| "The capture service stopped while applying settings.".to_owned())?;
    if cancelled_pending {
        warnings
            .push("A scheduled capture waiting for the previous time or area was skipped.".into());
    }
    if let Err(error) = synchronize_schedule(hwnd, &settings) {
        let (schedule, area, armed) = previous_schedule;
        if armed || schedule != settings.schedule || area != settings.schedule_area {
            warnings.push(format!("Scheduled capture paused: {error}"));
        }
    }
    if let Some(seconds) = rearm {
        if let Err(error) = rearm_interval_timer(hwnd, seconds) {
            warnings.push(error);
        }
    }
    publish_state();
    Ok(warnings)
}

#[cfg(target_os = "macos")]
fn commit_window_settings(
    settings: Settings,
    changes: geometry::SavedAreaChanges,
) -> Result<Vec<String>, String> {
    commit_window_settings_with(settings, changes, Settings::try_save)
}

#[cfg(target_os = "macos")]
pub(crate) fn edit_window(
    edit: crate::capture::WindowEdit,
) -> Result<crate::capture::WindowEditResult, String> {
    use crate::capture::{WindowEdit, WindowEditResult, WindowEffect};
    use crate::panel::{Applied, Pick};

    let mut settings = with_app(|app| app.settings.clone())
        .ok_or_else(|| "The capture service is unavailable.".to_owned())?;
    let (effect, warnings) = match edit {
        WindowEdit::Pick(id) => {
            let previous_fixed = settings.capture_window.fixed_at;
            let result = crate::panel::pick_unified(&mut settings, &id);
            let warnings = match &result {
                Pick::Changed => {
                    let mut changes = geometry::SavedAreaChanges::default();
                    if settings.capture_window.fixed_at.is_none()
                        && (previous_fixed.is_some() || settings.macos_fixed_area.is_some())
                    {
                        settings.macos_fixed_area = None;
                        changes.clear(SavedAreaKind::Fixed);
                    }
                    commit_window_settings(settings, changes)?
                }
                Pick::Presets(presets) => {
                    settings.presets = presets.clone();
                    crate::prefs::validate_hotkeys(&settings)?;
                    set_window_hotkeys(settings)?
                }
                _ => Vec::new(),
            };
            (WindowEffect::Pick(result), warnings)
        }
        WindowEdit::Flip(switch) => {
            crate::panel::flip(&mut settings, switch);
            let warnings = commit_window_settings(settings, geometry::SavedAreaChanges::default())?;
            (WindowEffect::Changed, warnings)
        }
        WindowEdit::Apply { id, values } => {
            let (tab, name) = crate::panel::capture_context(&id);
            let previous = settings.capture_window.tab;
            settings.capture_window.tab = tab.unwrap_or(1);
            let (result, changes) =
                crate::panel::prepare_apply(&mut settings, name, &values, runs_at_startup());
            settings.capture_window.tab = previous;
            let warnings = match &result {
                Applied::Problem(_) => Vec::new(),
                Applied::Presets(presets) => {
                    settings.presets = presets.clone();
                    crate::prefs::validate_hotkeys(&settings)?;
                    set_window_hotkeys(settings)?
                }
                _ => {
                    match &result {
                        Applied::MoveSchedule(at) | Applied::MoveScheduleWithWarning(at, _) => {
                            settings.schedule = Some(at.clone())
                        }
                        _ => {}
                    }
                    commit_window_settings(settings, changes)?
                }
            };
            (WindowEffect::Applied(result), warnings)
        }
        WindowEdit::ReplacePresets(presets) => {
            settings.presets = presets;
            crate::prefs::validate_hotkeys(&settings)?;
            let warnings = set_window_hotkeys(settings)?;
            (WindowEffect::Changed, warnings)
        }
        WindowEdit::Preferences {
            values,
            hotkeys,
            startup_initial,
        } => {
            let mut warnings = Vec::new();
            if !values.is_empty() {
                let startup = crate::prefs::apply_capture(&mut settings, &values, startup_initial);
                warnings.extend(commit_window_settings(
                    settings.clone(),
                    geometry::SavedAreaChanges::default(),
                )?);
                if let Some(on) = startup {
                    set_run_at_startup(on);
                    publish_state();
                }
            }
            if !hotkeys.is_empty() {
                let mut current = with_app(|app| app.settings.clone())
                    .ok_or_else(|| "The capture service is unavailable.".to_owned())?;
                match crate::prefs::apply_hotkeys(&mut current, &hotkeys) {
                    Ok(()) => warnings.extend(set_window_hotkeys(current)?),
                    Err(error) => warnings.push(format!("Hotkeys not changed: {error}")),
                }
            }
            (WindowEffect::Changed, warnings)
        }
    };
    Ok(WindowEditResult { effect, warnings })
}

/// Turns the daily scheduled capture on at "HH:MM", or off.
pub fn set_schedule(at: Option<String>) {
    #[cfg(target_os = "macos")]
    if at.is_some() {
        let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) else {
            return;
        };
        let validation = if at.as_deref().and_then(minutes_until).is_none() {
            Err(std::io::Error::other(
                "Review the scheduled time in Capture > Time delay.",
            ))
        } else {
            check_scheduled_area(&settings)
        };
        if let Err(error) = validation {
            notify(
                hwnd,
                "Scheduled capture not started",
                &error.to_string(),
                true,
            );
            return;
        }
    }
    let Some((hwnd, save_error, cancelled_pending)) = with_app(|app| {
        #[cfg(target_os = "macos")]
        let cancelled_pending =
            app.settings.schedule != at && app.deferred_schedule.take().is_some();
        #[cfg(windows)]
        let cancelled_pending = false;
        app.settings.schedule = at.clone();
        if at.is_some() {
            app.settings.schedule_default = at.clone();
        }
        #[cfg(windows)]
        app.settings.save();
        #[cfg(windows)]
        let error = None::<std::io::Error>;
        #[cfg(target_os = "macos")]
        let error = app.settings.try_save().err();
        (app.hwnd, error, cancelled_pending)
    }) else {
        return;
    };
    if let Some(error) = save_error {
        notify(
            hwnd,
            "Scheduled capture settings not saved",
            &format!("The change applies only until Jiffshot quits: {error}"),
            true,
        );
    }
    if cancelled_pending && at.is_some() {
        notify(
            hwnd,
            "Scheduled capture skipped",
            "The scheduled time changed while the capture was waiting.",
            true,
        );
    }
    #[cfg(windows)]
    if let Err(error) = arm_schedule(hwnd, at.as_deref()) {
        notify(hwnd, "Scheduled capture failed", &error.to_string(), true);
        publish_state();
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(settings) = with_app(|app| app.settings.clone()) {
        if let Err(error) = synchronize_schedule(hwnd, &settings) {
            notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
            publish_state();
            return;
        }
    }
    if let Some(at) = at {
        notify(
            hwnd,
            "Scheduled capture",
            &format!("The screen will be captured every day at {at}"),
            false,
        );
    }
    publish_state();
}

fn schedule_update(action: crate::capture::ScheduleAction, running: bool, at: Option<&str>) -> Option<Option<String>> {
    if action.should_toggle(running) {
        Some(match action {
            crate::capture::ScheduleAction::Start => Some(at.unwrap_or("09:00").to_owned()),
            crate::capture::ScheduleAction::Stop => None,
        })
    } else {
        None
    }
}

pub(crate) fn set_schedule_action(action: crate::capture::ScheduleAction) {
    let Some(at) = with_app(|app| schedule_update(action, app.settings.schedule.is_some(), app.settings.schedule_default.as_deref())).flatten() else { return };
    set_schedule(at);
}

pub(crate) fn move_schedule(at: String) {
    if let Some(true) = with_app(|app| app.settings.schedule.is_some()) {
        set_schedule(Some(at));
    }
}

pub fn current_settings() -> Settings {
    with_app(|app| app.settings.clone()).unwrap_or_default()
}

#[cfg(any(windows, test))]
fn replace_capture_settings(current: &mut Settings, settings: Settings) {
    let presets = std::mem::take(&mut current.presets);
    let schedule = current.schedule.take();
    *current = Settings { presets, schedule, ..settings };
}

fn interval_to_rearm(running: bool, old_seconds: u32, new_seconds: u32) -> Option<u32> {
    let new_seconds = new_seconds.max(1);
    (running && old_seconds.max(1) != new_seconds).then_some(new_seconds)
}

/// Keeps and saves settings edited in the Capture window.
pub fn apply_settings(settings: Settings) {
    #[cfg(target_os = "macos")]
    let rearm = with_app(|app| {
        (app.hwnd, interval_to_rearm(app.timelapse.is_some(), app.settings.interval_seconds, settings.interval_seconds))
    });
    #[cfg(target_os = "macos")]
    let previous_schedule = with_app(|app| {
        (
            app.settings.schedule.clone(),
            app.settings.schedule_area,
            app.schedule_armed,
        )
    });
    #[cfg(target_os = "macos")]
    let save_error = with_app(|app| {
        let cancelled_pending = app.deferred_schedule.as_ref().is_some_and(|pending| {
            settings.schedule.as_deref() != Some(pending.at.as_str())
                || settings.schedule_area != pending.area
        });
        if cancelled_pending {
            app.deferred_schedule = None;
        }
        let presets = std::mem::take(&mut app.settings.presets);
        app.settings = Settings {
            presets,
            ..settings
        };
        (
            app.hwnd,
            app.settings.try_save().err(),
            cancelled_pending && app.settings.schedule.is_some(),
        )
    });
    #[cfg(windows)]
    let rearm = with_app(|app| {
        let rearm = interval_to_rearm(app.timelapse.is_some(), app.settings.interval_seconds, settings.interval_seconds);
        replace_capture_settings(&mut app.settings, settings);
        app.settings.save();
        (app.hwnd, rearm)
    });
    #[cfg(target_os = "macos")]
    if let Some((hwnd, error, cancelled_pending)) = save_error {
        if let Some(error) = error {
            notify(
                hwnd,
                "Capture settings not saved",
                &format!("The changes apply only until Jiffshot quits: {error}"),
                true,
            );
        }
        if cancelled_pending {
            notify(
                hwnd,
                "Scheduled capture skipped",
                "The scheduled time or area changed while the capture was waiting.",
                true,
            );
        }
    }
    #[cfg(target_os = "macos")]
    if let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) {
        if let Err(error) = synchronize_schedule(hwnd, &settings) {
            if previous_schedule.is_some_and(|(schedule, area, armed)| {
                armed || schedule != settings.schedule || area != settings.schedule_area
            }) {
                notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
            }
        }
    }
    if let Some((hwnd, Some(seconds))) = rearm {
        if let Err(error) = rearm_interval_timer(hwnd, seconds) {
            notify(hwnd, "Time-lapse", &error, true);
        }
    }
    publish_state();
}

/// Keeps a recording's switches (as the toolbar left them) for the next one.
pub(crate) fn remember_recording(control: &crate::recorder::Control) {
    use std::sync::atomic::Ordering::Relaxed;
    with_app(|app| {
        let s = &mut app.settings;
        s.record_cursor = control.cursor.load(Relaxed);
        s.cursor_highlight = control.highlight.load(Relaxed);
        s.click_rings = control.clicks.load(Relaxed);
        s.webcam = control
            .webcam
            .load(Relaxed)
            .then(|| control.webcam_corner.lock().unwrap().clone());
        s.webcam_shape = if control.webcam_circle.load(Relaxed) {
            "circle"
        } else {
            "rectangle"
        }
        .to_owned();
        s.record_audio = match (
            control.system.load(Relaxed),
            control.microphone.load(Relaxed),
        ) {
            (true, true) => "both",
            (true, false) => "system",
            (false, true) => "microphone",
            (false, false) => "none",
        }
        .to_owned();
        s.save();
    });
    publish_state();
}

/// Replaces the presets and their hotkeys.
pub fn set_presets(presets: Vec<crate::settings::Preset>) {
    let Some(mut settings) = with_app(|app| app.settings.clone()) else {
        return;
    };
    settings.presets = presets;
    if let Err(error) = crate::prefs::validate_hotkeys(&settings) {
        if let Some(hwnd) = with_app(|app| app.hwnd) {
            notify(hwnd, "Jiffshot hotkeys", &error, true);
        }
        return;
    }
    set_hotkeys(settings);
}

struct HotkeyUpdate {
    hwnd: HWND,
    problems: Vec<String>,
    save_error: Option<std::io::Error>,
}

fn update_hotkeys(settings: Settings) -> Result<HotkeyUpdate, String> {
    let (hwnd, old_count) = with_app(|app| (app.hwnd, app.settings.presets.len()))
        .ok_or_else(|| "The capture service is unavailable.".to_owned())?;
    unsafe {
        KillTimer(hwnd, TIMER_HOTKEY_RETRY);
        for id in [HOTKEY_CAPTURE, HOTKEY_SCREEN, HOTKEY_REPEAT] {
            UnregisterHotKey(hwnd, id);
        }
        for i in 0..old_count {
            UnregisterHotKey(hwnd, HOTKEY_PRESET + i as i32);
        }
    }
    let desired_capture = settings.capture_key().to_owned();
    let (capture_key, mut problems) = register_hotkeys(hwnd, &settings);
    problems.extend(register_preset_hotkeys(hwnd, &settings.presets));
    let save_error = with_app(|app| {
        app.settings.capture_hotkey = settings.capture_hotkey;
        app.settings.screen_hotkey = settings.screen_hotkey;
        app.settings.repeat_hotkey = settings.repeat_hotkey;
        app.settings.presets = settings.presets;
        app.capture_key = capture_key.clone();
        #[cfg(target_os = "macos")]
        {
            app.settings.try_save().err()
        }
        #[cfg(windows)]
        {
            app.settings.save();
            None::<std::io::Error>
        }
    })
    .ok_or_else(|| "The capture service stopped while changing hotkeys.".to_owned())?;
    if capture_key != desired_capture {
        unsafe { SetTimer(hwnd, TIMER_HOTKEY_RETRY, 30_000, None) };
    }
    Ok(HotkeyUpdate {
        hwnd,
        problems,
        save_error,
    })
}

#[cfg(target_os = "macos")]
fn set_window_hotkeys(settings: Settings) -> Result<Vec<String>, String> {
    let update = update_hotkeys(settings)?;
    let mut warnings = Vec::new();
    if let Some(error) = update.save_error {
        warnings.push(format!(
            "Hotkeys were not saved and apply only until Jiffshot quits: {error}"
        ));
    }
    if !update.problems.is_empty() {
        for problem in &update.problems {
            log(problem);
        }
        warnings.push(update.problems.join(". "));
    }
    publish_state();
    Ok(warnings)
}

/// Replaces all global and preset hotkeys together on the capture thread.
pub fn set_hotkeys(settings: Settings) {
    let update = match update_hotkeys(settings) {
        Ok(update) => update,
        Err(error) => {
            log(&format!("Hotkeys not changed: {error}"));
            return;
        }
    };
    #[cfg(target_os = "macos")]
    if let Some(error) = update.save_error {
        notify(
            update.hwnd,
            "Hotkeys not saved",
            &format!("The changes apply only until Jiffshot quits: {error}"),
            true,
        );
    }
    #[cfg(windows)]
    let _ = update.save_error;
    if !update.problems.is_empty() {
        for problem in &update.problems {
            log(problem);
        }
        notify(
            update.hwnd,
            "Jiffshot hotkeys",
            &update.problems.join(". "),
            true,
        );
    }
    publish_state();
}

pub fn run_preset_now(index: usize) {
    run_preset(index);
}

fn dispatch_mode(mode: usize) {
    if matches!(
        mode,
        MODE_FULLSCREEN | MODE_CLIPBOARD | MODE_FILE | MODE_VIDEO_FULL
    ) {
        // A preset's choices apply to the overlay's captures only.
        with_app(|app| {
            (
                app.override_cursor,
                app.override_effects,
                app.override_share,
            ) = (None, None, None)
        });
    }
    match mode {
        usize::MAX => start_menu_capture(),
        MODE_STEPS => capture_steps(),
        MODE_FULLSCREEN => capture_screen_under_cursor(),
        MODE_CLIPBOARD => capture_from_clipboard(),
        MODE_FILE => capture_from_file(),
        MODE_VIDEO_FULL => record_screen_under_cursor(),
        m => capture_interactive(m),
    }
}

/// Records the whole screen under the pointer, without selecting.
fn record_screen_under_cursor() {
    let Some((hwnd, settings)) = with_app(|app| {
        if app.busy {
            return None;
        }
        app.busy = true;
        Some((app.hwnd, app.settings.clone()))
    })
    .flatten() else {
        return;
    };
    let result = (|| {
        let layout = CaptureLayout::current().map_err(|error| error.to_string())?;
        let monitor = layout
            .monitor_under_cursor()
            .map_err(|error| error.to_string())?
            .ok_or("no display is available for recording")?;
        let rect = monitor.rect;
        let layout = layout
            .with_monitor(&monitor)
            .map_err(|error| error.to_string())?;
        let path = crate::recorder::record_in_layout(rect, &settings, layout)?;
        Ok::<_, String>((path, rect))
    })();
    match result {
        Ok((Some(path), rect)) => spawn_video_finish(path, rect, settings, hwnd),
        Ok((None, _)) => {}
        Err(error) => {
            #[cfg(target_os = "macos")]
            crate::shutdown_macos::work_failed(&error);
            notify(hwnd, "Recording failed", &error, true);
        }
    }
    with_app(|app| app.busy = false);
}

/// Starts a capture by preset mode name, after the delay setting (image
/// captures; recordings count down themselves).
pub fn start_mode_after_delay(name: &str) {
    let mode = if name == "video" {
        MODE_VIDEO
    } else {
        mode_named(name).unwrap_or(MODE_IMAGE)
    };
    let Some((hwnd, delay)) = with_app(|app| (app.hwnd, app.settings.delay_seconds)) else {
        return;
    };
    if delay == 0 || mode == MODE_VIDEO {
        // Let the Capture window disappear from the screen first.
        with_app(|app| app.pending_mode = Some(PendingMode::Mode(mode)));
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_MODE, 150, None) };
        return;
    }
    with_app(|app| app.pending_mode = Some(PendingMode::Mode(mode)));
    notify(
        hwnd,
        "Jiffshot",
        &format!("Capturing in {delay} seconds…"),
        false,
    );
    // SAFETY: as above.
    unsafe { SetTimer(hwnd, TIMER_MODE, delay * 1000, None) };
}

#[cfg(target_os = "macos")]
pub fn select_scheduled_area() {
    let Some((hwnd, available)) =
        with_app(|app| (app.hwnd, !app.busy && app.pending_mode.is_none()))
    else {
        crate::show_capture_window();
        return;
    };
    if !available {
        notify(
            hwnd,
            "Scheduled area not selected",
            "Another capture is already in progress or waiting to start.",
            true,
        );
        crate::show_capture_window();
        return;
    }
    with_app(|app| app.pending_mode = Some(PendingMode::Mode(MODE_SCHEDULE_AREA)));
    if unsafe { SetTimer(hwnd, TIMER_MODE, 150, None) } == 0 {
        with_app(|app| app.pending_mode = None);
        notify(
            hwnd,
            "Scheduled area not selected",
            "Could not start the selection overlay.",
            true,
        );
        crate::show_capture_window();
    }
}

/// Captures exactly `rect` after the delay setting.
pub fn capture_rect_after_delay(rect: Rect) {
    let Some((hwnd, delay)) = with_app(|app| (app.hwnd, app.settings.delay_seconds)) else {
        return;
    };
    #[cfg(target_os = "macos")]
    {
        let ticket = match saved_area_ticket(SavedAreaKind::Fixed, rect) {
            Ok(ticket) => ticket,
            Err(error) => {
                notify(hwnd, "Capture failed", &error.to_string(), true);
                return;
            }
        };
        with_app(|app| app.pending_mode = Some(PendingMode::Fixed(ticket)));
    }
    #[cfg(windows)]
    with_app(|app| app.pending_mode = Some(PendingMode::Fixed(rect)));
    // SAFETY: a timer on our own window.
    unsafe { SetTimer(hwnd, TIMER_MODE, (delay * 1000).max(150), None) };
}

fn run_pending_mode() {
    let Some((hwnd, settings, pending)) =
        with_app(|app| (app.hwnd, app.settings.clone(), app.pending_mode.take()))
    else {
        return;
    };
    let capture = match pending {
        Some(PendingMode::Mode(mode)) => return dispatch_mode(mode),
        #[cfg(target_os = "macos")]
        Some(PendingMode::Fixed(ticket)) => capture_fixed_rect(&settings, &ticket),
        #[cfg(windows)]
        Some(PendingMode::Fixed(rect)) => capture_fixed_rect(rect, settings.include_cursor),
        None => return,
    };
    match capture {
        Ok(capture) => spawn_finish(capture, settings, hwnd),
        Err(error) => notify(hwnd, "Capture failed", &error.to_string(), true),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn capture_fixed_rect(
    settings: &Settings,
    ticket: &SavedAreaTicket,
) -> std::io::Result<workflow::Capture> {
    let rect = ticket.rect();
    if rect.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid fixed capture dimensions",
        ));
    }
    let current = settings
        .capture_window
        .fixed_at
        .map(|(x, y)| Rect::new(x, y, settings.fixed_size.0, settings.fixed_size.1));
    if current != Some(rect) {
        return Err(std::io::Error::other(
            "The fixed capture position changed while the capture was waiting.",
        ));
    }
    ticket.validate()?;
    let (area, layout) =
        ticket
            .layout()
            .fixed_at(rect.x, rect.y, rect.width as u32, rect.height as u32)?;
    workflow::immediate_in_layout_with_cursor(
        CaptureKind::Region,
        area,
        settings.include_cursor,
        settings.cursor_as_object,
        &layout,
    )
}

#[cfg(windows)]
fn capture_fixed_rect(rect: Rect, include_cursor: bool) -> std::io::Result<workflow::Capture> {
    workflow::immediate(CaptureKind::Region, rect, include_cursor)
}

fn run_preset(index: usize) {
    let Some((hwnd, preset)) = with_app(|app| (app.hwnd, app.settings.presets.get(index).cloned()))
    else {
        return;
    };
    let Some(preset) = preset else { return };
    if preset.delay > 0 && with_app(|app| app.pending_preset.is_none()).unwrap_or(false) {
        with_app(|app| app.pending_preset = Some(index));
        notify(
            hwnd,
            &preset.name,
            &format!("Capturing in {} seconds…", preset.delay),
            false,
        );
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_PRESET, preset.delay * 1000, None) };
        return;
    }
    with_app(|app| {
        app.override_cursor = Some(preset.cursor);
        app.override_effects = Some(preset.effects.clone());
        app.override_share = preset.share.clone();
    });
    dispatch_mode(mode_named(&preset.mode).unwrap_or(MODE_IMAGE));
}

fn register_preset_hotkeys(hwnd: HWND, presets: &[crate::settings::Preset]) -> Vec<String> {
    let mut problems = Vec::new();
    for (i, p) in presets.iter().enumerate() {
        if let Some(key) = p.hotkey.as_deref() {
            match parse_hotkey(key)
                .filter(|(mods, vk)| keys::supported(*vk) && !keys::reserved(*mods, *vk))
            {
                Some((mods, vk)) if register_hotkey(hwnd, HOTKEY_PRESET + i as i32, mods, vk) => {}
                Some(_) => problems.push(format!(
                    "{} (preset {}) is taken",
                    keys::display(key),
                    p.name
                )),
                None => problems.push(format!(
                    "{} (preset {}) is not a supported hotkey",
                    keys::display(key),
                    p.name
                )),
            }
        }
    }
    problems
}

/// Captures the menus open right now.
fn capture_menus() {
    let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) else {
        return;
    };
    match modes::menus() {
        Ok(Some(frame)) => {
            let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
            spawn_finish(
                workflow::from_frame(CaptureKind::Menu, rect, frame),
                settings,
                hwnd,
            );
        }
        Ok(None) => notify(
            hwnd,
            "Menu capture",
            "No open menu was found. Open the menu during the countdown.",
            true,
        ),
        Err(error) => notify(hwnd, "Menu capture failed", &error.to_string(), true),
    }
}

fn start_menu_capture() {
    if let Some(hwnd) = with_app(|app| app.hwnd) {
        notify(
            hwnd,
            "Menu capture",
            "Open the menu to capture; it is captured in 3 seconds.",
            false,
        );
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_MENU, 3000, None) };
    }
}

/// Captures a rectangle without the overlay.
fn capture_now(kind: CaptureKind, region: CaptureRegion) {
    let Some((hwnd, settings)) = with_app(|app| (app.hwnd, app.settings.clone())) else {
        return;
    };
    #[cfg(target_os = "macos")]
    let capture = workflow::immediate_in_layout_with_cursor(
        kind,
        region.rect,
        settings.include_cursor,
        settings.cursor_as_object,
        &region.layout,
    );
    #[cfg(windows)]
    let capture =
        workflow::immediate_in_layout(kind, region.rect, settings.include_cursor, &region.layout);
    match capture {
        Ok(capture) => {
            with_app(|app| app.last_region = Some(region));
            spawn_finish(capture, settings, hwnd);
        }
        Err(error) => notify(hwnd, "Capture failed", &error.to_string(), true),
    }
}

fn capture_screen_under_cursor() {
    let Some(hwnd) = with_app(|app| app.hwnd) else {
        return;
    };
    let layout = match CaptureLayout::current() {
        Ok(layout) => layout,
        Err(error) => {
            notify(hwnd, "Capture failed", &error.to_string(), true);
            return;
        }
    };
    match layout.monitor_under_cursor() {
        Ok(Some(monitor)) => match layout.with_monitor(&monitor) {
            Ok(layout) => capture_now(
                CaptureKind::Monitor,
                CaptureRegion {
                    rect: monitor.rect,
                    layout,
                },
            ),
            Err(error) => notify(hwnd, "Capture failed", &error.to_string(), true),
        },
        Ok(None) => {}
        Err(error) => notify(hwnd, "Capture failed", &error.to_string(), true),
    }
}

fn repeat_last_region() {
    match with_app(|app| (app.hwnd, app.last_region.clone())) {
        Some((hwnd, Some(region))) => {
            if let Err(error) = region.layout.validate() {
                with_app(|app| app.last_region = None);
                notify(hwnd, "Repeat capture failed", &error.to_string(), true);
                publish_state();
                return;
            }
            capture_now(CaptureKind::Region, region);
        }
        Some((_, None)) => capture_interactive(MODE_IMAGE),
        None => {}
    }
}

fn start_delayed() {
    if let Some((hwnd, seconds)) = with_app(|app| (app.hwnd, app.settings.delay_seconds.max(1))) {
        notify(
            hwnd,
            "Jiffshot",
            &format!("Capturing in {seconds} seconds…"),
            false,
        );
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_DELAY, seconds * 1000, None) };
    }
}

pub(crate) fn open_library() {
    let root = jiffshot_capture::config::library_root();
    let _ = std::fs::create_dir_all(&root);
    #[cfg(windows)]
    {
        use jiffshot_capture::wide;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let path = wide(&root.to_string_lossy());
        // SAFETY: opens a folder in Explorer; all strings are NUL-terminated.
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(target_os = "macos")]
    let _ = jiffshot_capture::os::open_path(&root);
}

/// Runs a tray menu command (an `ID_*`), as the Avalonia tray menu asks
/// through [`TrayAction::post`].
fn run_menu_command(hwnd: HWND, command: usize) {
    let toggle = |f: fn(&mut Settings) -> &mut bool| {
        with_app(|app| {
            let value = f(&mut app.settings);
            *value = !*value;
            app.settings.save();
        });
    };
    match command {
        // Let the menu close before the screen is frozen.
        ID_CAPTURE => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, 0, 0);
        },
        ID_SCROLLING => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_SCROLLING, 0);
        },
        ID_PANORAMIC => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_PANORAMIC, 0);
        },
        ID_TEXT => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_TEXT, 0);
        },
        ID_VIDEO => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_VIDEO, 0);
        },
        ID_MENU => start_menu_capture(),
        ID_FREEHAND => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_FREEHAND, 0);
        },
        ID_MULTI => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_MULTI, 0);
        },
        ID_FIXED => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_FIXED, 0);
        },
        ID_SCROLL_H => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_SCROLL_H, 0);
        },
        ID_STEPS => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_STEPS, 0);
        },
        ID_TIMELAPSE => toggle_timelapse(),
        ID_CAPWIN => crate::show_capture_window(),
        ID_SCROLL_BOTH => unsafe {
            PostMessageW(hwnd, WM_APP_CAPTURE, MODE_SCROLL_BOTH, 0);
        },
        ID_FROM_CLIPBOARD => capture_from_clipboard(),
        ID_FROM_FILE => capture_from_file(),
        ID_SCHEDULE => {
            // Turns it off, or on at `schedule_default` (09:00 unless set).
            #[cfg(target_os = "macos")]
            {
                let Some(schedule) = with_app(|app| {
                    if app.settings.schedule.is_some() {
                        None
                    } else {
                        Some(
                            app.settings
                                .schedule_default
                                .clone()
                                .unwrap_or_else(|| "09:00".to_owned()),
                        )
                    }
                }) else {
                    return;
                };
                set_schedule(schedule);
            }
            #[cfg(windows)]
            {
                let schedule = with_app(|app| {
                    app.settings.schedule = match app.settings.schedule.take() {
                        Some(_) => None,
                        None => Some(
                            app.settings
                                .schedule_default
                                .clone()
                                .unwrap_or_else(|| "09:00".to_owned()),
                        ),
                    };
                    app.settings.save();
                    app.settings.schedule.clone()
                })
                .flatten();
                if let Err(error) = arm_schedule(hwnd, schedule.as_deref()) {
                    notify(hwnd, "Scheduled capture failed", &error.to_string(), true);
                    return;
                }
                if let Some(at) = schedule {
                    notify(
                        hwnd,
                        "Scheduled capture",
                        &format!("The screen will be captured every day at {at}"),
                        false,
                    );
                }
            }
        }
        ID_STARTUP => set_run_at_startup(!runs_at_startup()),
        ID_WIDGET => set_widget(!current_settings().show_widget),
        id if id >= ID_PRESET => run_preset(id - ID_PRESET),
        ID_DELAYED => start_delayed(),
        ID_SCREEN => capture_screen_under_cursor(),
        ID_REPEAT => repeat_last_region(),
        ID_CLIPBOARD => toggle(|s| &mut s.copy_to_clipboard),
        ID_EDITOR => toggle(|s| &mut s.open_in_editor),
        ID_CURSOR => toggle(|s| &mut s.include_cursor),
        ID_LIBRARY => open_library(),
        #[cfg(windows)]
        ID_EXIT => unsafe { PostQuitMessage(0) },
        #[cfg(target_os = "macos")]
        ID_EXIT => request_exit(),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        #[cfg(target_os = "macos")]
        WM_APP_REQUEST_EXIT => {
            request_exit();
            0
        }
        WM_HOTKEY => {
            match wparam as i32 {
                HOTKEY_CAPTURE => capture_interactive(MODE_IMAGE),
                HOTKEY_SCREEN => capture_screen_under_cursor(),
                HOTKEY_REPEAT => repeat_last_region(),
                id if id >= HOTKEY_PRESET => run_preset((id - HOTKEY_PRESET) as usize),
                _ => {}
            }
            0
        }
        WM_APP_RUN => {
            // SAFETY: posted by `on_capture_thread` with a leaked box.
            let run = Box::from_raw(lparam as *mut Box<dyn FnOnce() + Send>);
            run();
            0
        }
        #[cfg(target_os = "macos")]
        WM_APP_SCROLL_DONE => {
            with_app(|app| app.busy = false);
            0
        }
        WM_APP_COMMAND => {
            run_menu_command(hwnd, wparam);
            publish_state();
            0
        }
        WM_APP_CAPTURE => {
            if wparam == MODE_STEPS {
                capture_steps();
            } else if wparam == OPEN_CAPTURE_WINDOW {
                crate::show_capture_window();
            } else {
                dispatch_mode(wparam);
            }
            0
        }
        WM_APP_CAPTURE_LATER => {
            with_app(|app| app.pending = Some(wparam));
            SetTimer(hwnd, TIMER_INITIAL, (lparam as u32).max(1), None);
            0
        }
        WM_APP_MENU => {
            start_menu_capture();
            0
        }
        WM_APP_DONE => {
            // SAFETY: posted by `spawn_finish` with a leaked Box<Outcome>.
            let outcome = Box::from_raw(lparam as *mut Outcome);
            if outcome.is_error || !outcome.opened_editor {
                let title = if outcome.is_error {
                    "Capture saved with problems"
                } else {
                    "Capture saved"
                };
                notify(hwnd, title, &outcome.message, outcome.is_error);
            }
            0
        }
        WM_TIMER if wparam == TIMER_DELAY => {
            KillTimer(hwnd, TIMER_DELAY);
            capture_interactive(MODE_IMAGE);
            0
        }
        WM_TIMER if wparam == TIMER_MENU => {
            KillTimer(hwnd, TIMER_MENU);
            capture_menus();
            0
        }
        WM_TIMER if wparam == TIMER_MODE => {
            KillTimer(hwnd, TIMER_MODE);
            run_pending_mode();
            0
        }
        WM_TIMER if wparam == TIMER_SCHEDULE => {
            schedule_tick();
            0
        }
        WM_TIMER if wparam == TIMER_TIMELAPSE => {
            timelapse_tick();
            0
        }
        WM_TIMER if wparam == TIMER_PRESET => {
            KillTimer(hwnd, TIMER_PRESET);
            if let Some(i) = with_app(|app| app.pending_preset).flatten() {
                run_preset(i);
                with_app(|app| app.pending_preset = None);
            }
            0
        }
        WM_TIMER if wparam == TIMER_INITIAL => {
            KillTimer(hwnd, TIMER_INITIAL);
            if let Some(mode) = with_app(|app| app.pending.take()).flatten() {
                if mode == OPEN_CAPTURE_WINDOW {
                    crate::show_capture_window();
                } else {
                    dispatch_mode(mode);
                }
            }
            0
        }
        WM_TIMER if wparam == TIMER_HOTKEY_RETRY => {
            retry_capture_key(hwnd);
            0
        }
        #[cfg(target_os = "macos")]
        WM_TIMER if wparam == TIMER_STARTUP_STATUS => {
            refresh_startup_status();
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(target_os = "macos")]
fn request_exit() {
    if let Some(hooks) = crate::hooks() {
        (hooks.quit)();
    } else {
        log("could not quit: the application is unavailable");
    }
}

/// Runs the tray app until Exit, first starting the capture `initial` asks
/// for (a mode, or `usize::MAX` for menus). Returns the exit code.
pub fn run(
    initial: Option<usize>,
    #[cfg(target_os = "macos")] control: &mut Option<jiffshot_control::Server>,
) -> i32 {
    let hwnd = sys::create_message_window(CLASS, "Jiffshot Capture", wndproc);
    if hwnd.is_null() {
        log("could not create the tray window");
        return 1;
    }

    #[cfg(windows)]
    sys::serve_instance(CLASS);
    #[cfg(target_os = "macos")]
    if let Err(error) = sys::serve_instance(CLASS) {
        log(&format!(
            "could not start the capture instance listener: {error}"
        ));
        eprintln!("could not start the capture instance listener: {error}");
        return 1;
    }
    let settings = Settings::load();
    #[cfg(target_os = "macos")]
    restore_saved_coordinates(&settings);
    let (capture_key, mut problems) = register_hotkeys(hwnd, &settings);
    problems.extend(register_preset_hotkeys(hwnd, &settings.presets));
    // Which pause and stop keys recordings will use, for the windows that
    // show them.
    crate::recorder::check_keys();
    log(&format!("capture side started; capture key {capture_key}"));
    for problem in &problems {
        log(problem);
    }
    if !problems.is_empty() {
        notify(hwnd, "Jiffshot hotkeys", &problems.join(". "), true);
    }
    if capture_key != settings.capture_key() {
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_HOTKEY_RETRY, 30_000, None) };
    }
    APP.with(|app| {
        *app.borrow_mut() = Some(App {
            hwnd,
            settings,
            busy: false,
            last_region: None,
            capture_key,
            pending: initial,
            timelapse: None,
            pending_preset: None,
            override_cursor: None,
            override_effects: None,
            override_share: None,
            scheduled_fired: None,
            #[cfg(target_os = "macos")]
            schedule_armed: false,
            #[cfg(target_os = "macos")]
            deferred_schedule: None,
            pending_mode: None,
        })
    });
    let settings_now = with_app(|app| app.settings.clone()).unwrap_or_default();
    #[cfg(windows)]
    if let Err(error) = arm_schedule(hwnd, settings_now.schedule.as_deref()) {
        notify(hwnd, "Scheduled capture failed", &error.to_string(), true);
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = synchronize_schedule(hwnd, &settings_now) {
        notify(hwnd, "Scheduled capture paused", &error.to_string(), true);
    }
    publish_state();
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a timer on our own capture window refreshes user-controlled login-item state.
        if unsafe { SetTimer(hwnd, TIMER_STARTUP_STATUS, 10_000, None) } == 0 {
            notify(hwnd, "Open at login status", "Could not refresh the login-item status automatically. Reopen Preferences to check it.", true);
        }
    }
    if initial.is_some() {
        // Give the window that asked (the editor) time to get out of the way.
        // SAFETY: a timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_INITIAL, 350, None) };
    }

    #[cfg(target_os = "macos")]
    match crate::control_macos::start() {
        Ok(server) => *control = Some(server),
        Err(error) => log(&format!("control: not available: {error}")),
    }

    // SAFETY: the standard message loop for this thread, then cleanup of the
    // icon and hotkeys registered above.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for id in [HOTKEY_CAPTURE, HOTKEY_SCREEN, HOTKEY_REPEAT] {
            UnregisterHotKey(hwnd, id);
        }
    }
    log("tray stopped");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use jiffshot_capture::wide;
    #[cfg(windows)]
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, PeekMessageW, HWND_MESSAGE, PM_REMOVE};

    #[cfg(target_os = "macos")]
    #[test]
    fn clipboard_capture_validates_pixels_and_preserves_alpha() {
        use jiffshot_capture::clipboard::Pasted;
        assert!(frame_from_clipboard(None).unwrap().is_none());
        assert!(frame_from_clipboard(Some(Pasted::Text("text".into())))
            .unwrap()
            .is_none());
        let frame = frame_from_clipboard(Some(Pasted::Image {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 128, 0, 0, 255, 255],
        }))
        .unwrap()
        .unwrap();
        assert!(frame.alpha);
        assert_eq!(frame.pixels, [0, 0, 255, 128, 255, 0, 0, 255]);
        let invalid = frame_from_clipboard(Some(Pasted::Image {
            width: 2,
            height: 1,
            rgba: vec![0; 4],
        }));
        assert!(invalid.err().unwrap().contains("Invalid clipboard image"));
        let oversized = frame_from_clipboard(Some(Pasted::Image {
            width: u32::MAX,
            height: 1,
            rgba: Vec::new(),
        }));
        assert!(oversized.err().unwrap().contains("too large"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clipboard_capture_reports_bad_files_and_uses_the_first_valid_image() {
        use jiffshot_capture::clipboard::Pasted;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("jiffshot-clipboard-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let missing = root.join("missing.png");
        let path = root.join("valid.png");
        let mut source = jiffshot_capture::Frame::new(1, 1);
        source.alpha = true;
        source.pixels.copy_from_slice(&[0, 0, 255, 128]);
        source.save_png(&path).unwrap();

        let result = frame_from_clipboard(Some(Pasted::Files(vec![missing.clone(), path.clone()])))
            .unwrap()
            .unwrap();
        assert_eq!(result.pixels, [0, 0, 255, 128]);
        assert!(result.alpha);
        let error = frame_from_clipboard(Some(Pasted::Files(vec![missing.clone()])))
            .err()
            .unwrap();
        assert!(error.contains("Could not open") && error.contains("missing.png"));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    unsafe extern "system" fn test_wndproc(
        _hwnd: HWND,
        _msg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> LRESULT {
        0
    }

    #[cfg(target_os = "macos")]
    fn test_app(hwnd: HWND, settings: Settings) -> App {
        App {
            hwnd,
            settings,
            busy: false,
            last_region: None,
            capture_key: String::new(),
            pending: None,
            timelapse: None,
            pending_preset: None,
            override_cursor: None,
            override_effects: None,
            override_share: None,
            scheduled_fired: None,
            schedule_armed: false,
            deferred_schedule: None,
            pending_mode: None,
        }
    }

    #[test]
    fn hotkeys_parse() {
        use crate::sys::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, VK_SNAPSHOT};
        assert_eq!(
            parse_hotkey("Ctrl+Shift+1"),
            Some((MOD_CONTROL | MOD_SHIFT, b'1' as u16))
        );
        assert_eq!(parse_hotkey("alt + F9"), Some((MOD_ALT, 0x78)));
        assert_eq!(parse_hotkey("PrtScn"), Some((0, VK_SNAPSHOT)));
        assert_eq!(parse_hotkey("Ctrl+"), None);
        assert_eq!(parse_hotkey("Hyper+K"), None);
    }

    #[test]
    fn preset_modes_resolve() {
        for p in crate::settings::Settings::default().presets {
            assert!(mode_named(&p.mode).is_some(), "{}", p.mode);
        }
        assert_eq!(mode_named("steps"), Some(MODE_STEPS));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn startup_refresh_only_publishes_changes() {
        PUBLISHED_STARTUP.with(|published| *published.borrow_mut() = None);
        let disabled = StartupStatus::default();
        let approval = StartupStatus {
            requested: true,
            notice: StartupNotice::NeedsApproval,
            detail: Some("approval".into()),
        };
        assert!(remember_startup(&disabled));
        assert!(!remember_startup(&disabled));
        assert!(remember_startup(&approval));
        assert!(!remember_startup(&approval));
        PUBLISHED_STARTUP.with(|published| *published.borrow_mut() = None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn delayed_fixed_capture_rejects_reconfirmation_and_layout_changes() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        geometry::confirm_saved_area(SavedAreaKind::Fixed, rect).unwrap();
        let ticket = saved_area_ticket(SavedAreaKind::Fixed, rect).unwrap();
        let mut settings = Settings::default();
        settings.capture_window.fixed_at = Some((rect.x, rect.y));
        settings.fixed_size = (rect.width, rect.height);
        let changed = ticket.clone().invalidated_for_test();
        assert!(capture_fixed_rect(&settings, &changed)
            .err()
            .unwrap()
            .to_string()
            .contains("display configuration changed"));
        settings.capture_window.fixed_at = None;
        assert!(capture_fixed_rect(&settings, &ticket)
            .err()
            .unwrap()
            .to_string()
            .contains("position changed"));
        settings.capture_window.fixed_at = Some((rect.x, rect.y));
        geometry::confirm_saved_area(SavedAreaKind::Fixed, rect).unwrap();
        assert!(capture_fixed_rect(&settings, &ticket)
            .err()
            .unwrap()
            .to_string()
            .contains("confirmation"));
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn panel_confirmation_and_settings_stay_together_when_saving_fails() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let old = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let next = Rect::new(old.x + 1, old.y + 1, 101, 81);
        let mut settings = Settings::default();
        settings.capture_window.fixed = true;
        settings.capture_window.fixed_at = Some((old.x, old.y));
        settings.fixed_size = (old.width, old.height);
        settings.macos_fixed_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Fixed, old).unwrap());
        let old_ticket = saved_area_ticket(SavedAreaKind::Fixed, old).unwrap();
        let hwnd = sys::create_message_window("JiffshotPanelCommitTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, settings.clone())));
        let values = [
            true.into(),
            next.width.into(),
            next.height.into(),
            true.into(),
            next.x.into(),
            next.y.into(),
            false.into(),
        ];

        let mut candidate = settings.clone();
        let (result, changes) =
            crate::panel::prepare_apply(&mut candidate, "selection", &values, false);
        assert_eq!(result, crate::panel::Applied::Changed);
        assert!(
            old_ticket.validate().is_ok(),
            "preparing the panel must not install a confirmation"
        );
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_err());
        let error = commit_window_settings_with(candidate, changes, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only settings",
            ))
        })
        .unwrap_err();
        assert!(error.contains("read-only settings"));
        assert_eq!(
            serde_json::to_value(current_settings()).unwrap(),
            serde_json::to_value(&settings).unwrap()
        );
        assert!(old_ticket.validate().is_ok());
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_err());

        let mut candidate = settings;
        let (result, changes) =
            crate::panel::prepare_apply(&mut candidate, "selection", &values, false);
        assert_eq!(result, crate::panel::Applied::Changed);
        let mut persisted = None;
        assert!(
            commit_window_settings_with(candidate.clone(), changes, |value| {
                persisted = Some(value.clone());
                Ok(())
            })
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            serde_json::to_value(persisted.unwrap()).unwrap(),
            serde_json::to_value(current_settings()).unwrap()
        );
        assert_eq!(
            current_settings().macos_fixed_area,
            candidate.macos_fixed_area
        );
        assert!(old_ticket.validate().is_err());
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_ok());
        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unrelated_capture_options_remain_usable_when_the_settings_file_is_read_only() {
        let hwnd = sys::create_message_window("JiffshotPanelSessionOnlyTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, Settings::default())));
        let mut candidate = current_settings();
        candidate.capture_window.tab = 2;
        let warnings =
            commit_window_settings_with(candidate, geometry::SavedAreaChanges::default(), |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "read-only settings",
                ))
            })
            .unwrap();
        assert_eq!(current_settings().capture_window.tab, 2);
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("only until Jiffshot quits")
                && warning.contains("read-only settings")));
        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saved_schedule_restores_only_for_its_matching_persisted_grid() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let mut settings = Settings::default();
        settings.schedule = Some("09:00".into());
        settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap());
        let json = serde_json::to_string(&settings).unwrap();
        let mut restored: Settings = serde_json::from_str(&json).unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        assert!(check_scheduled_area(&restored).is_err());
        restore_saved_coordinates(&restored);
        assert!(check_scheduled_area(&restored).is_ok());

        restored
            .macos_schedule_area
            .as_mut()
            .unwrap()
            .coordinate_space[0] ^= 1;
        restore_saved_coordinates(&restored);
        assert!(check_scheduled_area(&restored).is_err());
        restored.schedule_area = None;
        assert!(check_scheduled_area(&restored).is_ok());
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn selected_schedule_area_uses_the_overlay_layout_and_replaces_the_old_ticket() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let old = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let selected = Rect::new(monitor.rect.x + 1, monitor.rect.y + 1, 90, 70);
        let confirmation = geometry::confirm_saved_area(SavedAreaKind::Scheduled, old).unwrap();
        let old_ticket = saved_area_ticket(SavedAreaKind::Scheduled, old).unwrap();
        let mut settings = Settings::default();
        settings.copy_to_clipboard = false;
        settings.schedule = Some("09:00".into());
        settings.schedule_area = Some((old.x, old.y, old.width, old.height));
        settings.macos_schedule_area = Some(confirmation);
        let hwnd = sys::create_message_window("JiffshotScheduleSelectionTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, settings)));
        synchronize_schedule(hwnd, &with_app(|app| app.settings.clone()).unwrap()).unwrap();
        with_app(|app| {
            app.deferred_schedule = Some(
                DeferredScheduledCapture::new(&app.settings, std::time::Instant::now()).unwrap(),
            );
        });

        let selection = overlay::Selection::Region(selected);
        let error =
            save_scheduled_selection(hwnd, &selection, layout.clone().invalidated_for_test())
                .unwrap_err();
        assert!(error.to_string().contains("display configuration changed"));
        assert!(old_ticket.validate().is_ok());
        assert_eq!(
            with_app(|app| app.settings.schedule_area).flatten(),
            Some((old.x, old.y, old.width, old.height))
        );

        let error = save_scheduled_selection_with(hwnd, &selection, layout.clone(), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "settings are read-only",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("settings are read-only"));
        assert!(old_ticket.validate().is_ok());
        assert_eq!(
            with_app(|app| app.settings.schedule_area).flatten(),
            Some((old.x, old.y, old.width, old.height))
        );
        assert!(with_app(|app| app.deferred_schedule.is_some()).unwrap());

        let result = save_scheduled_selection(hwnd, &selection, layout).unwrap();
        assert!(result.paused.is_none());
        assert!(!result.session_only);
        assert!(result.cancelled_pending);
        assert!(old_ticket.validate().is_err());
        assert!(with_app(|app| app.deferred_schedule.is_none()).unwrap());
        assert!(saved_area_ticket(SavedAreaKind::Scheduled, selected).is_ok());
        assert!(with_app(|app| app.schedule_armed).unwrap());
        let persisted = Settings::load();
        assert!(!persisted.copy_to_clipboard);
        assert_eq!(
            persisted.schedule_area,
            Some((selected.x, selected.y, selected.width, selected.height))
        );
        assert_eq!(
            persisted
                .macos_schedule_area
                .as_ref()
                .map(|proof| proof.rect),
            Some(selected)
        );
        unsafe { KillTimer(hwnd, TIMER_SCHEDULE) };
        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn selected_schedule_area_warns_when_display_identity_is_session_only() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let old = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let selected = Rect::new(monitor.rect.x + 1, monitor.rect.y + 1, 90, 70);
        let mut settings = Settings::default();
        settings.schedule = Some("09:00".into());
        settings.schedule_area = Some((old.x, old.y, old.width, old.height));
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, old).unwrap());
        let old_ticket = saved_area_ticket(SavedAreaKind::Scheduled, old).unwrap();
        let hwnd =
            sys::create_message_window("JiffshotSessionScheduleSelectionTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, settings)));
        synchronize_schedule(hwnd, &with_app(|app| app.settings.clone()).unwrap()).unwrap();
        with_app(|app| {
            app.deferred_schedule = Some(
                DeferredScheduledCapture::new(&app.settings, std::time::Instant::now()).unwrap(),
            )
        });
        let selection = overlay::Selection::Region(selected);

        let failed = geometry::with_missing_uuid_for_test(|| {
            save_scheduled_selection_with(hwnd, &selection, layout.clone(), |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "settings are read-only",
                ))
            })
        });
        let (title, message, warning) = scheduled_selection_notice(failed);
        assert_eq!(title, "Scheduled area not saved");
        assert!(warning && message.contains("settings are read-only"));
        assert!(!message.contains(geometry::SESSION_ONLY_AREA_WARNING));
        assert!(old_ticket.validate().is_ok());
        assert_eq!(
            with_app(|app| app.settings.schedule_area).flatten(),
            Some((old.x, old.y, old.width, old.height))
        );
        assert!(with_app(|app| app.deferred_schedule.is_some()).unwrap());

        let mut persisted = None;
        let result = geometry::with_missing_uuid_for_test(|| {
            save_scheduled_selection_with(hwnd, &selection, layout, |settings| {
                persisted = Some(settings.clone());
                Ok(())
            })
        })
        .unwrap();
        assert!(result.session_only && result.cancelled_pending && result.paused.is_none());
        let (title, message, warning) = scheduled_selection_notice(Ok(result));
        assert_eq!(title, "Scheduled area saved");
        assert!(
            warning
                && message.contains(geometry::SESSION_ONLY_AREA_WARNING)
                && message.contains("skipped")
        );
        assert!(old_ticket.validate().is_err());
        assert!(saved_area_ticket(SavedAreaKind::Scheduled, selected).is_ok());
        assert!(with_app(|app| app.schedule_armed).unwrap());
        let settings = persisted.unwrap();
        assert!(settings.macos_schedule_area.as_ref().unwrap().session_only);
        assert_eq!(
            settings.schedule_area,
            Some((selected.x, selected.y, selected.width, selected.height))
        );

        let (title, message, warning) = scheduled_selection_notice(Ok(SavedScheduledSelection {
            session_only: true,
            cancelled_pending: false,
            paused: Some(std::io::Error::other("display changed")),
        }));
        assert_eq!(title, "Scheduled area saved; capture paused");
        assert!(
            warning
                && message.contains("display changed")
                && message.contains(geometry::SESSION_ONLY_AREA_WARNING)
        );
        restore_saved_coordinates(&settings);
        assert!(check_scheduled_area(&settings).is_err());
        unsafe { KillTimer(hwnd, TIMER_SCHEDULE) };
        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_area_picker_does_not_replace_a_pending_capture() {
        let hwnd = sys::create_message_window("JiffshotSchedulePickerTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, Settings::default())));
        with_app(|app| app.pending_mode = Some(PendingMode::Mode(MODE_IMAGE)));
        select_scheduled_area();
        assert!(
            with_app(|app| matches!(app.pending_mode, Some(PendingMode::Mode(MODE_IMAGE))))
                .unwrap()
        );
        with_app(|app| app.pending_mode = None);
        select_scheduled_area();
        assert!(with_app(|app| matches!(
            app.pending_mode,
            Some(PendingMode::Mode(MODE_SCHEDULE_AREA))
        ))
        .unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_MODE) }, 1);
        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saved_schedule_confirmation_survives_process_restart() {
        const CHILD: &str = "JIFFSHOT_TEST_SAVED_AREA_CHILD";
        const CHILD_BINARY: &str = "JIFFSHOT_TEST_SAVED_AREA_CHILD_BINARY";
        const CHILD_HOST: &str = "JIFFSHOT_TEST_SAVED_AREA_CHILD_HOST";
        if let Ok(expected) = std::env::var(CHILD) {
            let settings = Settings::load();
            assert!(!settings.copy_to_clipboard);
            assert_eq!(settings.fixed_size, (641, 481));
            assert_eq!(
                settings.macos_schedule_area.is_none(),
                matches!(expected.as_str(), "malformed" | "malformed-flag")
            );
            if expected == "tampered-missing" {
                geometry::with_missing_uuid_for_test(|| restore_saved_coordinates(&settings));
            } else {
                restore_saved_coordinates(&settings);
            }
            let scheduled_ok = matches!(
                expected.as_str(),
                "matching" | "legacy-v2" | "fixed-session-only"
            );
            assert_eq!(check_scheduled_area(&settings).is_ok(), scheduled_ok);
            let (x, y) = settings.capture_window.fixed_at.unwrap();
            let fixed = Rect::new(x, y, settings.fixed_size.0, settings.fixed_size.1);
            let fixed_ok = !matches!(
                expected.as_str(),
                "session-only"
                    | "fixed-session-only"
                    | "tampered-flag"
                    | "tampered-missing"
                    | "missing-flag"
            );
            assert_eq!(
                saved_area_ticket(SavedAreaKind::Fixed, fixed).is_ok(),
                fixed_ok
            );
            return;
        }

        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let fixed = Rect::new(monitor.rect.x, monitor.rect.y, 641, 481);
        let mut settings = Settings::default();
        settings.copy_to_clipboard = false;
        settings.fixed_size = (641, 481);
        settings.capture_window.fixed = true;
        settings.capture_window.fixed_at = Some((fixed.x, fixed.y));
        settings.macos_fixed_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Fixed, fixed).unwrap());
        settings.schedule = Some("09:00".into());
        settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap());
        let (session_fixed, session_schedule) = geometry::with_missing_uuid_for_test(|| {
            (
                geometry::confirm_saved_area(SavedAreaKind::Fixed, fixed).unwrap(),
                geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap(),
            )
        });
        let root = std::env::temp_dir().join(format!(
            "jiffshot-saved-area-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        for expected in [
            "matching",
            "legacy-v2",
            "mismatched",
            "mismatched-uuid",
            "old-version",
            "malformed",
            "malformed-flag",
            "session-only",
            "fixed-session-only",
            "scheduled-session-only",
            "tampered-flag",
            "tampered-missing",
            "missing-flag",
        ] {
            let mut candidate = settings.clone();
            match expected {
                "mismatched" => {
                    candidate
                        .macos_schedule_area
                        .as_mut()
                        .unwrap()
                        .coordinate_space[0] ^= 1
                }
                "mismatched-uuid" => {
                    candidate
                        .macos_schedule_area
                        .as_mut()
                        .unwrap()
                        .coordinate_space[3] ^= 1
                }
                "old-version" => candidate.macos_schedule_area.as_mut().unwrap().version = 1,
                "session-only" | "tampered-flag" | "tampered-missing" | "missing-flag" => {
                    candidate.macos_fixed_area = Some(session_fixed.clone());
                    candidate.macos_schedule_area = Some(session_schedule.clone());
                    if expected != "session-only" {
                        candidate.macos_fixed_area.as_mut().unwrap().session_only = false;
                        candidate.macos_schedule_area.as_mut().unwrap().session_only = false;
                    }
                }
                "fixed-session-only" => candidate.macos_fixed_area = Some(session_fixed.clone()),
                "scheduled-session-only" => {
                    candidate.macos_schedule_area = Some(session_schedule.clone())
                }
                _ => {}
            }
            let mut root_value = serde_json::json!({ "capture": candidate });
            if expected == "malformed" {
                root_value["capture"]["macos_schedule_area"] = serde_json::json!("invalid");
            }
            if expected == "malformed-flag" {
                root_value["capture"]["macos_schedule_area"]["session_only"] =
                    serde_json::json!("false");
            }
            if matches!(expected, "legacy-v2" | "missing-flag") {
                root_value["capture"]["macos_fixed_area"]
                    .as_object_mut()
                    .unwrap()
                    .remove("session_only");
                root_value["capture"]["macos_schedule_area"]
                    .as_object_mut()
                    .unwrap()
                    .remove("session_only");
            }
            let json = serde_json::to_vec(&root_value).unwrap();
            std::fs::write(root.join(jiffshot_capture::config::FILE), json).unwrap();
            let binary = std::env::var_os(CHILD_BINARY)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_exe().unwrap());
            let mut child = std::process::Command::new(binary);
            child
                .args([
                    "--exact",
                    "tray::tests::saved_schedule_confirmation_survives_process_restart",
                    "--nocapture",
                ])
                .env(CHILD, expected)
                .env("JIFFSHOT_CONFIG_DIR", &root)
                .env("JIFFSHOT_LOCAL_DIR", &root)
                .env("JIFFSHOT_LIBRARY", &root)
                .env("APPDATA", &root)
                .env("LOCALAPPDATA", &root);
            if let Some(host) = std::env::var_os(CHILD_HOST) {
                child.env("RUSTOLONIA_HOST_LIB", host);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "child {expected} failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if matches!(expected, "malformed" | "malformed-flag") {
                assert!(String::from_utf8_lossy(&output.stderr)
                    .contains("Malformed macOS scheduled saved-area confirmation"));
            }
        }
        std::fs::remove_file(root.join(jiffshot_capture::config::FILE)).unwrap();
        let log = root.join("capture.log");
        if log.exists() {
            std::fs::remove_file(log).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
        geometry::clear_saved_area(SavedAreaKind::Fixed).unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scheduled_capture_pauses_and_rearms_after_explicit_confirmation() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let mut settings = Settings::default();
        settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));
        let hwnd = sys::create_message_window("JiffshotScheduleTest", "", test_wndproc);
        APP.with(|app| *app.borrow_mut() = Some(test_app(hwnd, settings)));

        set_schedule(Some("09:00".into()));
        assert_eq!(
            with_app(|app| app.settings.schedule.clone()).flatten(),
            None
        );
        assert!(!with_app(|app| app.schedule_armed).unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 0);

        let confirmation = geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap();
        with_app(|app| app.settings.macos_schedule_area = Some(confirmation));
        let mut candidate = with_app(|app| app.settings.clone()).unwrap();
        candidate.schedule = Some("09:00".into());
        assert!(synchronize_schedule(std::ptr::null_mut(), &candidate).is_err());
        assert!(!with_app(|app| app.schedule_armed).unwrap());
        set_schedule(Some("09:00".into()));
        assert!(with_app(|app| app.schedule_armed).unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 1);
        arm_schedule(hwnd, Some("09:00")).unwrap();

        geometry::invalidate_saved_area_for_test(SavedAreaKind::Scheduled).unwrap();
        schedule_tick();
        assert!(!with_app(|app| app.schedule_armed).unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 0);
        assert_eq!(
            with_app(|app| app.settings.schedule.clone())
                .flatten()
                .as_deref(),
            Some("09:00")
        );

        let mut settings = with_app(|app| app.settings.clone()).unwrap();
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap());
        apply_settings(settings);
        assert!(with_app(|app| app.schedule_armed).unwrap());
        geometry::invalidate_saved_area_for_test(SavedAreaKind::Scheduled).unwrap();
        capture_scheduled(hwnd, with_app(|app| app.settings.clone()).unwrap());
        assert!(!with_app(|app| app.schedule_armed).unwrap());
        assert!(with_app(|app| app.scheduled_fired).flatten().is_none());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 0);
        assert_eq!(
            with_app(|app| app.settings.schedule.clone())
                .flatten()
                .as_deref(),
            Some("09:00")
        );

        let mut settings = with_app(|app| app.settings.clone()).unwrap();
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap());
        apply_settings(settings);
        assert!(with_app(|app| app.schedule_armed).unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 1);
        arm_schedule(hwnd, Some("09:00")).unwrap();
        set_schedule(None);
        assert!(!with_app(|app| app.schedule_armed).unwrap());
        assert_eq!(unsafe { KillTimer(hwnd, TIMER_SCHEDULE) }, 0);

        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn deferred_schedule_keeps_its_original_area_and_expires() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let mut settings = Settings::default();
        settings.schedule = Some("09:00".into());
        settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));
        settings.macos_schedule_area =
            Some(geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap());

        let now = std::time::Instant::now();
        let pending = DeferredScheduledCapture::new(&settings, now).unwrap();
        assert!(pending.validate(&settings, now).is_ok());
        assert!(pending
            .validate(&settings, now + std::time::Duration::from_secs(121))
            .unwrap_err()
            .to_string()
            .contains("two minutes"));
        let mut changed = settings.clone();
        changed.schedule = Some("09:01".into());
        assert!(pending.validate(&changed, now).is_err());
        changed = settings.clone();
        changed.schedule_area = None;
        assert!(pending.validate(&changed, now).is_err());
        geometry::confirm_saved_area(SavedAreaKind::Scheduled, rect).unwrap();
        assert!(pending
            .validate(&settings, now)
            .unwrap_err()
            .to_string()
            .contains("confirmation"));
        geometry::clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scheduled_tick_defers_once_while_busy_and_disabling_cancels_it() {
        let _guard = geometry::SAVED_AREAS_TEST_LOCK.lock().unwrap();
        let mut settings = Settings::default();
        settings.schedule = Some("09:00".into());
        let hwnd = sys::create_message_window("JiffshotDeferredScheduleTest", "", test_wndproc);
        let mut app = test_app(hwnd, settings);
        app.busy = true;
        APP.with(|slot| *slot.borrow_mut() = Some(app));
        synchronize_schedule(hwnd, &with_app(|app| app.settings.clone()).unwrap()).unwrap();

        let settings = with_app(|app| app.settings.clone()).unwrap();
        assert!(defer_busy_schedule(hwnd, &settings, true));
        let queued = with_app(|app| app.deferred_schedule.as_ref().map(|pending| pending.queued))
            .flatten()
            .expect("a due capture must wait");
        assert_eq!(with_app(|app| app.scheduled_fired).flatten(), Some(queued));
        assert!(defer_busy_schedule(hwnd, &settings, true));
        assert_eq!(
            with_app(|app| app.deferred_schedule.as_ref().map(|pending| pending.queued)).flatten(),
            Some(queued)
        );
        with_app(|app| {
            app.deferred_schedule.as_mut().unwrap().queued =
                std::time::Instant::now() - std::time::Duration::from_secs(121);
        });
        assert!(defer_busy_schedule(hwnd, &settings, false));
        assert!(with_app(|app| app.deferred_schedule.is_none()).unwrap());
        assert!(defer_busy_schedule(hwnd, &settings, true));
        assert!(with_app(|app| app.deferred_schedule.is_some()).unwrap());
        let mut changed = settings.clone();
        changed.schedule = Some("09:01".into());
        apply_settings(changed);
        assert!(with_app(|app| app.deferred_schedule.is_none()).unwrap());
        apply_settings(settings.clone());
        assert!(
            with_app(|app| app.deferred_schedule.is_none()).unwrap(),
            "restoring settings must not revive the old capture"
        );
        assert!(defer_busy_schedule(hwnd, &settings, true));
        set_schedule(None);
        assert!(with_app(|app| app.deferred_schedule.is_none()).unwrap());
        assert!(!with_app(|app| app.schedule_armed).unwrap());

        APP.with(|app| *app.borrow_mut() = None);
        unsafe { DefWindowProcW(hwnd, sys::WM_CLOSE, 0, 0) };
    }

    #[test]
    fn step_guide_saves_editable_content_and_reports_write_failure() {
        use jiffshot_doc::object::Kind;
        let frame = jiffshot_capture::Frame::new(100, 100);
        let doc = crate::stepcap::guide(&[crate::stepcap::Step {
            frame,
            area: Rect::new(0, 0, 100, 100),
            mapping: None,
            click: (50, 50),
            #[cfg(target_os = "macos")]
            click_precise: None,
            right: false,
            description: "Click the test button.".into(),
        }])
        .unwrap();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-step-guide-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let settings = Settings {
            open_in_editor: false,
            ..Settings::default()
        };
        let path = root.join("guide.png");
        let outcome = finish_step_guide(&doc, &path, 1, &settings);
        assert!(!outcome.is_error, "{}", outcome.message);
        let editable = path.with_extension(jiffshot_capture::library::EDITS_EXT);
        let loaded = jiffshot_doc::io::load_document(&editable).unwrap();
        assert!(loaded.objects.iter().any(|object| matches!(&object.kind, Kind::Text { text, .. } if text == "Click the test button.")));
        #[cfg(target_os = "macos")]
        {
            let mut missing = doc.clone();
            for object in &mut missing.objects {
                object.walk_mut(&mut |object| {
                    object.style.font = "Jiffshot unavailable guide face 80ed6b".into()
                });
            }
            let warned_path = root.join("warned.png");
            let warned = finish_step_guide(&missing, &warned_path, 1, &settings);
            assert!(!warned.is_error);
            assert!(warned
                .message
                .contains("Jiffshot unavailable guide face 80ed6b"));
            assert!(warned.message.contains("using Helvetica Neue"));
            assert!(warned.message.contains("1 steps saved"));
            std::fs::remove_file(warned_path.with_extension(jiffshot_capture::library::EDITS_EXT))
                .unwrap();
        }
        let blocked = root.join("not-a-directory");
        std::fs::write(&blocked, b"").unwrap();
        let outcome = finish_step_guide(&doc, &blocked.join("guide.png"), 1, &settings);
        assert!(outcome.is_error);
        assert!(outcome
            .message
            .contains("editable guide could not be saved"));
        std::fs::remove_file(editable).unwrap();
        std::fs::remove_file(blocked).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn select_create_choices_dispatch_or_cancel() {
        for (index, label, mode) in [
            (0, "Image", MODE_IMAGE),
            (1, "Video", MODE_VIDEO),
            (2, "Scrolling (auto)", MODE_SCROLLING),
            (3, "Panoramic (manual)", MODE_PANORAMIC),
        ] {
            assert_eq!(crate::panel::SELECT_CREATE_CHOICES[index], label);
            assert_eq!(select_create_mode(Some(index)), Some(mode), "{label}");
        }
        assert_eq!(crate::panel::SELECT_CREATE_CHOICES[4], "Cancel");
        assert_eq!(select_create_mode(Some(4)), None);
        assert_eq!(select_create_mode(Some(5)), None);
        assert_eq!(select_create_mode(None), None);
    }

    #[test]
    fn interval_actions_are_idempotent_when_state_changes_before_dispatch() {
        use crate::capture::IntervalAction::{Start, Stop};
        for (action, running, should_toggle) in [(Start, false, true), (Start, true, false), (Stop, true, true), (Stop, false, false)] {
            assert_eq!(action.should_toggle(running), should_toggle, "{action:?} while running={running}");
        }
    }

    #[test]
    fn schedule_actions_are_idempotent_when_state_changes_before_dispatch() {
        use crate::capture::ScheduleAction::{Start, Stop};
        assert_eq!(schedule_update(Start, false, Some("12:45")), Some(Some("12:45".into())));
        assert_eq!(schedule_update(Start, false, None), Some(Some("09:00".into())));
        assert_eq!(schedule_update(Start, true, Some("12:45")), None, "a stale Start must not cancel an active schedule");
        assert_eq!(schedule_update(Stop, true, Some("12:45")), Some(None));
        assert_eq!(schedule_update(Stop, false, Some("12:45")), None, "a stale Cancel must not start a schedule");
    }

    #[test]
    fn applying_stale_capture_settings_preserves_the_live_schedule() {
        let mut current = Settings { schedule: Some("12:45".into()), ..Settings::default() };
        let presets = current.presets.clone();
        let incoming = Settings { schedule: None, share: "email".into(), presets: Vec::new(), ..Settings::default() };
        replace_capture_settings(&mut current, incoming);
        assert_eq!(current.schedule.as_deref(), Some("12:45"));
        assert_eq!(current.share, "email");
        assert_eq!(current.presets, presets);

        current.schedule = None;
        let incoming = Settings { schedule: Some("08:00".into()), ..Settings::default() };
        replace_capture_settings(&mut current, incoming);
        assert_eq!(current.schedule, None, "a canceled schedule is not restored by a stale view");
    }

    #[test]
    fn changing_a_running_interval_rearms_only_when_its_cadence_changes() {
        assert_eq!(interval_to_rearm(false, 10, 30), None);
        assert_eq!(interval_to_rearm(true, 10, 10), None);
        assert_eq!(interval_to_rearm(true, 0, 1), None);
        assert_eq!(interval_to_rearm(true, 10, 30), Some(30));
        assert_eq!(interval_to_rearm(true, 10, 0), Some(1));
    }

    #[cfg(windows)]
    #[test]
    fn rearming_interval_changes_the_next_timer_tick() {
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                // SAFETY: this test owns the timer and its message-only window.
                unsafe {
                    KillTimer(self.0, TIMER_TIMELAPSE);
                    DestroyWindow(self.0);
                }
            }
        }

        // SAFETY: a hidden message-only window using the system STATIC class.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null(), "the test window should be created");
        let window = TestWindow(hwnd);
        let has_timer_message = || {
            // SAFETY: the message is initialized and queried only for this test's window.
            unsafe {
                let mut message: MSG = std::mem::zeroed();
                PeekMessageW(&mut message, window.0, WM_TIMER, WM_TIMER, PM_REMOVE) != 0 && message.wParam == TIMER_TIMELAPSE
            }
        };

        assert!(arm_interval_timer(window.0, 1), "the first interval should start");
        assert!(arm_interval_timer(window.0, 10), "the longer interval should replace it");
        std::thread::sleep(std::time::Duration::from_millis(1300));
        assert!(!has_timer_message(), "the original one-second interval must no longer fire");

        assert!(arm_interval_timer(window.0, 1), "the shorter interval should replace it");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !has_timer_message() {
            assert!(Instant::now() < deadline, "the new one-second interval should fire");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
