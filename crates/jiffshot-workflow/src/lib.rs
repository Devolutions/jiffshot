//! Jiffshot's capture side: the tray, hotkeys, the selection overlay,
//! recording, and the workflow after a capture. `jiffshot.exe` runs it on its
//! own thread ([`start_engine`]) next to the editor, in the same process.
//!
//! Command lines `jiffshot.exe` understands (see [`run_command`]):
//!
//! ```text
//! jiffshot                              tray (or, if already running, the Capture window)
//! jiffshot FILE...                      open files in the editor
//! jiffshot --capture|--scrolling|--panoramic|--text|--record|--menu|--steps|--freehand|--window
//!                                       start that capture (starting Jiffshot if needed)
//! jiffshot --grab X,Y,W,H OUT           capture a screen rectangle to a PNG
//! jiffshot --grab screen OUT            capture the monitor under the cursor
//! jiffshot --monitors                   list monitors
//! jiffshot --list-windows               list detected capture targets
//! jiffshot --selftest DIR               drive the overlay offscreen, write frames and timings
//! jiffshot --save-test X,Y,W,H DIR      save to a library in DIR and open the editor
//! jiffshot --ocr IMAGE.png              print the text recognized in an image
//! jiffshot --scroll-selftest DIR        scrolling capture of a generated page (moves the pointer)
//! jiffshot --hotkey-selftest            test macOS hotkey registration and in-process delivery
//! jiffshot --step-selftest DIR          exercise macOS click observation on a synthetic fixture
//! jiffshot --record-selftest DIR SECS   record a generated page
//! jiffshot --quit                       stop the running instance
//! jiffshot --write-icon OUT.ico         write the program icon (assets/jiffshot.ico)
//! jiffshot --write-logo SIZE OUT.png    render the logo as a square PNG
//! ```
//!
//! The running instance also serves `jiffshot-cli` (scripts and AI agents)
//! over a named pipe on Windows (see `control.rs`) and a Unix socket on macOS
//! (see `control_macos.rs`); see `docs/cli.md`.

#![cfg(any(windows, target_os = "macos"))]

#[cfg(any(target_os = "macos", test))]
mod audio_timeline;
#[cfg(windows)]
mod control;
#[cfg(target_os = "macos")]
mod control_macos;
mod geometry;
mod hud;
mod icon;
pub mod keys;
mod modes;
mod overlay;
pub mod panel;
pub mod prefs;
mod recorder;
mod screendraw;
#[cfg_attr(target_os = "macos", path = "selftest_scroll_mac.rs")]
mod selftest_scroll;
pub mod settings;
#[cfg(target_os = "macos")]
mod shutdown_macos;
mod stepcap;
mod sys;
mod tray;
pub mod ui_api;
mod vfx;
mod workflow;

#[cfg(target_os = "macos")]
pub use workflow::frame_from_pixmap;

use jiffshot_capture::screen::{self, Monitor};
use jiffshot_capture::targets::Targets;
use jiffshot_capture::{Frame, Rect};
use overlay::Overlay;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::HWND;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// What the capture side asks of the app it runs in.
pub struct Hooks {
    /// Opens a saved capture in the editor; false when it could not.
    pub open_in_editor: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    /// Shows the editor window (its library when nothing is open).
    pub show_editor: Box<dyn Fn() + Send + Sync>,
    /// The tray's Exit, or the engine stopping: ends the whole app.
    pub quit: Box<dyn Fn() + Send + Sync>,
    /// A notice for the user (title, text, whether it reports a problem).
    pub notify: Box<dyn Fn(&str, &str, bool) + Send + Sync>,
    /// What the tray menu should show, sent after anything in it changes.
    pub state: Box<dyn Fn(TrayState) + Send + Sync>,
    /// Login-item state may change in System Settings without other settings changing.
    #[cfg(target_os = "macos")]
    pub startup_status: Box<dyn Fn(StartupStatus) + Send + Sync>,
    /// Shows the Capture window.
    pub show_capture_window: Box<dyn Fn() + Send + Sync>,
    /// Shows text recognized by Grab Text.
    pub show_text: Box<dyn Fn(String) + Send + Sync>,
    /// The capture panels and outlines on screen, as JSON (see `hud.rs`),
    /// sent after every change; the panels' buttons answer through
    /// [`answer_panel`].
    pub hud: Box<dyn Fn(String) + Send + Sync>,
    /// The selection overlay to show, as JSON (session, desktop, monitors;
    /// see `overlay.rs`), or empty to take it down. The app draws it and
    /// feeds it input through [`ui_api`].
    pub overlay: Box<dyn Fn(String) + Send + Sync>,
    /// Screen Draw to show over an area while recording, as JSON (session,
    /// area), or empty to end it.
    pub screen_draw: Box<dyn Fn(String) + Send + Sync>,
}

/// Button `button` of capture panel `id` was pressed (`None`: the panel was
/// closed some other way); safe from any thread.
pub fn answer_panel(id: u64, button: Option<usize>) {
    hud::answer(id, button);
}

/// A button of toolbar `id` (the recording toolbar) sent `action`; safe
/// from any thread.
pub fn panel_action(id: u64, action: &str) {
    hud::act(id, action.to_owned());
}

/// The capture side's settings as last published (see [`settings_snapshot`]).
#[cfg(not(target_os = "macos"))]
pub(crate) static SNAPSHOT: std::sync::Mutex<Option<settings::Settings>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "macos")]
pub(crate) static CAPTURE_SNAPSHOT: std::sync::Mutex<Option<(settings::Settings, TrayState)>> =
    std::sync::Mutex::new(None);

/// The capture side's settings, as of its last change.
pub fn settings_snapshot() -> settings::Settings {
    #[cfg(not(target_os = "macos"))]
    let settings = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(target_os = "macos")]
    let settings = capture_state_snapshot().map(|(settings, _)| settings);
    settings.unwrap_or_else(settings::Settings::load)
}

/// The capture settings and tray state from the same publication.
#[cfg(target_os = "macos")]
pub fn capture_state_snapshot() -> Option<(settings::Settings, TrayState)> {
    CAPTURE_SNAPSHOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(windows)]
fn is_local_window(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let mut process = 0;
    // SAFETY: this only queries the owner of the window handle.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process);
        process == GetCurrentProcessId()
    }
}

/// Runs `f` on this process's capture thread; dropped if it is not running.
pub fn on_capture_thread(f: impl FnOnce() + Send + 'static) {
    let _ = post_capture_thread(f);
}

fn post_capture_thread(f: impl FnOnce() + Send + 'static) -> bool {
    let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    let raw = Box::into_raw(boxed);
    if !sys::post_to_class(tray::CLASS, tray::WM_APP_RUN, 0, raw as isize) {
        // SAFETY: the post failed, so its handler never takes the box.
        drop(unsafe { Box::from_raw(raw) });
        return false;
    }
    true
}

/// Shows the Capture window (through the app).
pub(crate) fn show_capture_window() {
    if let Some(hooks) = hooks() {
        (hooks.show_capture_window)();
    }
}

/// Shows Grab Text's result (through the app).
pub(crate) fn show_text(text: &str) {
    if let Some(hooks) = hooks() {
        (hooks.show_text)(text.to_owned());
    }
}

/// What the Capture window asks the capture side for; each runs on its
/// thread.
pub mod capture {
    use super::{on_capture_thread, settings::Preset, settings::Settings, tray};

    #[cfg(target_os = "macos")]
    use std::sync::{Arc, Mutex};

    #[cfg(target_os = "macos")]
    pub enum WindowEdit {
        Pick(String),
        Flip(super::panel::Switch),
        Apply {
            id: String,
            values: Vec<serde_json::Value>,
        },
        ReplacePresets(Vec<Preset>),
        Preferences {
            values: Vec<serde_json::Value>,
            hotkeys: Vec<serde_json::Value>,
            startup_initial: bool,
        },
    }

    #[cfg(target_os = "macos")]
    pub enum WindowEffect {
        Pick(super::panel::Pick),
        Applied(super::panel::Applied),
        Changed,
    }

    #[cfg(target_os = "macos")]
    pub struct WindowEditResult {
        pub effect: WindowEffect,
        pub warnings: Vec<String>,
    }

    #[cfg(target_os = "macos")]
    enum EditState {
        Queued,
        Running,
        Finished(Option<Result<WindowEditResult, String>>),
        Cancelled,
    }

    #[cfg(target_os = "macos")]
    #[derive(Clone)]
    pub struct WindowEditTicket(Arc<Mutex<EditState>>);

    #[cfg(target_os = "macos")]
    pub enum WindowEditStatus {
        Queued,
        Running,
        Finished(Result<WindowEditResult, String>),
        Cancelled,
        Consumed,
    }

    #[cfg(target_os = "macos")]
    impl WindowEditTicket {
        pub fn status(&self) -> Result<WindowEditStatus, String> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| "the capture settings request is unavailable".to_owned())?;
            Ok(match &mut *state {
                EditState::Queued => WindowEditStatus::Queued,
                EditState::Running => WindowEditStatus::Running,
                EditState::Finished(result) => result
                    .take()
                    .map_or(WindowEditStatus::Consumed, WindowEditStatus::Finished),
                EditState::Cancelled => WindowEditStatus::Cancelled,
            })
        }

        /// A queued request can be cancelled without a later edit taking effect.
        pub fn cancel_queued(&self) -> Result<bool, String> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| "the capture settings request is unavailable".to_owned())?;
            if matches!(*state, EditState::Queued) {
                *state = EditState::Cancelled;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    /// Applies a Capture-window action against the capture thread's current settings.
    #[cfg(target_os = "macos")]
    pub fn edit_window(
        edit: WindowEdit,
        finished: impl FnOnce() + Send + 'static,
    ) -> Result<WindowEditTicket, String> {
        let ticket = WindowEditTicket(Arc::new(Mutex::new(EditState::Queued)));
        let worker = ticket.clone();
        if !super::post_capture_thread(move || {
            {
                let Ok(mut state) = worker.0.lock() else {
                    super::log(
                        "Could not start a capture settings request: its state is unavailable",
                    );
                    finished();
                    return;
                };
                if matches!(*state, EditState::Cancelled) {
                    return;
                }
                *state = EditState::Running;
            }
            let result = tray::edit_window(edit);
            match worker.0.lock() {
                Ok(mut state) => *state = EditState::Finished(Some(result)),
                Err(_) => super::log(
                    "Could not finish a capture settings request: its state is unavailable",
                ),
            }
            finished();
        }) {
            return Err("The capture service is unavailable.".into());
        }
        Ok(ticket)
    }

    /// The interval action the Capture window showed when it was pressed.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum IntervalAction {
        Start,
        Stop,
    }

    impl IntervalAction {
        pub(crate) fn should_toggle(self, running: bool) -> bool {
            matches!((self, running), (Self::Start, false) | (Self::Stop, true))
        }
    }

    /// The daily-schedule action the Capture window showed when it was pressed.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ScheduleAction {
        Start,
        Stop,
    }

    impl ScheduleAction {
        pub(crate) fn should_toggle(self, running: bool) -> bool {
            matches!((self, running), (Self::Start, false) | (Self::Stop, true))
        }
    }

    /// Saves the window's settings.
    pub fn apply(s: Settings) {
        on_capture_thread(move || tray::apply_settings(s));
    }

    /// Saves the settings and starts the window's capture (see
    /// `tray::start_from_panel`).
    pub fn start(s: Settings) {
        on_capture_thread(move || tray::start_from_panel(s));
    }

    /// Starts with the capture thread's committed settings, rather than a UI snapshot.
    #[cfg(target_os = "macos")]
    pub fn start_current() -> bool {
        super::post_capture_thread(tray::start_from_current_panel)
    }

    #[cfg(target_os = "macos")]
    pub fn set_interval_current(action: IntervalAction) -> bool {
        super::post_capture_thread(move || tray::set_timelapse(action))
    }

    #[cfg(target_os = "macos")]
    pub fn set_schedule_current(action: ScheduleAction) -> bool {
        super::post_capture_thread(move || tray::set_schedule_action(action))
    }

    /// Applies the settings and starts or stops the interval as displayed.
    pub fn set_interval(s: Settings, action: IntervalAction) {
        on_capture_thread(move || {
            tray::apply_settings(s);
            tray::set_timelapse(action);
        });
    }

    /// Stops a running interval capture, without starting one if it already stopped.
    pub fn stop_timelapse() {
        on_capture_thread(tray::stop_timelapse);
    }

    /// Applies the settings and starts or cancels the displayed daily schedule.
    pub fn set_schedule(s: Settings, action: ScheduleAction) {
        on_capture_thread(move || {
            tray::apply_settings(s);
            tray::set_schedule_action(action);
        });
    }

    /// Cancels a daily schedule without starting one if it already stopped.
    pub fn cancel_schedule() {
        on_capture_thread(|| tray::set_schedule_action(ScheduleAction::Stop));
    }

    pub fn run_preset(index: usize) {
        on_capture_thread(move || tray::run_preset_now(index));
    }

    pub fn set_presets(presets: Vec<Preset>) {
        on_capture_thread(move || tray::set_presets(presets));
    }

    pub fn set_hotkeys(settings: Settings) {
        on_capture_thread(move || tray::set_hotkeys(settings));
    }

    pub fn step_capture() {
        on_capture_thread(|| tray::start_mode_after_delay("steps"));
    }

    /// Moves a daily schedule that is on to `at`.
    pub fn move_schedule(at: String) {
        on_capture_thread(move || tray::move_schedule(at));
    }

    /// Opens the selection overlay to choose a daily scheduled capture area.
    #[cfg(target_os = "macos")]
    pub fn select_scheduled_area() -> bool {
        super::post_capture_thread(tray::select_scheduled_area)
    }

    pub fn set_run_at_startup(on: bool) {
        if !super::post_capture_thread(move || {
            tray::set_run_at_startup(on);
            tray::publish_state();
        }) {
            if let Some(hooks) = super::hooks() {
                (hooks.notify)(
                    "Open at login not changed",
                    "The capture service is unavailable.",
                    true,
                );
            } else {
                super::log("Open at login not changed: the capture service is unavailable");
            }
        }
    }

    pub fn runs_at_startup() -> bool {
        tray::runs_at_startup()
    }

    pub fn startup_status() -> super::StartupStatus {
        tray::startup_status()
    }

    pub fn open_library() {
        on_capture_thread(tray::open_library);
    }

    #[cfg(all(test, target_os = "macos"))]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        unsafe extern "system" fn dispatch(
            hwnd: crate::sys::HWND,
            message: u32,
            wparam: crate::sys::WPARAM,
            lparam: crate::sys::LPARAM,
        ) -> crate::sys::LRESULT {
            if message == tray::WM_APP_RUN {
                let run = unsafe { Box::from_raw(lparam as *mut Box<dyn FnOnce() + Send>) };
                run();
                0
            } else {
                unsafe { crate::sys::DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }

        fn dispatch_one() {
            let mut msg: crate::sys::MSG = unsafe { std::mem::zeroed() };
            assert_ne!(
                unsafe {
                    crate::sys::PeekMessageW(
                        &mut msg,
                        std::ptr::null_mut(),
                        0,
                        0,
                        crate::sys::PM_REMOVE,
                    )
                },
                0
            );
            unsafe { crate::sys::DispatchMessageW(&msg) };
        }

        #[test]
        fn window_edit_acknowledges_execution_and_cancels_only_queued_work() {
            let hwnd = crate::sys::create_message_window(tray::CLASS, "", dispatch);
            let callbacks = Arc::new(AtomicUsize::new(0));
            let called = callbacks.clone();
            let cancelled = edit_window(
                WindowEdit::Flip(crate::panel::Switch::Clipboard),
                move || {
                    called.fetch_add(1, Ordering::Relaxed);
                },
            )
            .unwrap();
            assert!(matches!(
                cancelled.status().unwrap(),
                WindowEditStatus::Queued
            ));
            assert!(cancelled.cancel_queued().unwrap());
            dispatch_one();
            assert!(matches!(
                cancelled.status().unwrap(),
                WindowEditStatus::Cancelled
            ));
            assert_eq!(callbacks.load(Ordering::Relaxed), 0);

            let called = callbacks.clone();
            let executed = edit_window(
                WindowEdit::Flip(crate::panel::Switch::Clipboard),
                move || {
                    called.fetch_add(1, Ordering::Relaxed);
                },
            )
            .unwrap();
            dispatch_one();
            assert_eq!(callbacks.load(Ordering::Relaxed), 1);
            assert!(!executed.cancel_queued().unwrap());
            assert!(
                matches!(executed.status().unwrap(), WindowEditStatus::Finished(Err(error)) if error.contains("unavailable"))
            );
            assert!(matches!(
                executed.status().unwrap(),
                WindowEditStatus::Consumed
            ));
            unsafe { crate::sys::DefWindowProcW(hwnd, crate::sys::WM_CLOSE, 0, 0) };

            let called = callbacks.clone();
            assert!(edit_window(
                WindowEdit::Flip(crate::panel::Switch::Clipboard),
                move || {
                    called.fetch_add(1, Ordering::Relaxed);
                }
            )
            .is_err());
            assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        }
    }
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Capture modes the editor asks for with [`request_capture`].
pub use tray::{
    StartupNotice, StartupStatus, TrayAction, TrayState, MODE_IMAGE, MODE_SCROLLING, MODE_TEXT,
    MODE_VIDEO, OPEN_CAPTURE_WINDOW,
};

/// The app's hooks; `None` for command-line modes, which run without an app.
pub(crate) fn hooks() -> Option<&'static Hooks> {
    HOOKS.get()
}

/// Appends a line to `%LOCALAPPDATA%\Jiffshot\capture.log` (and stderr).
pub fn log(line: &str) {
    let line = format!("{} {line}", jiffshot_capture::library::timestamp());
    eprintln!("{line}");
    let dir = jiffshot_capture::paths::local_dir();
    {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("capture.log"))
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn parse_rect(text: &str) -> Option<Rect> {
    let v: Vec<i32> = text
        .split(',')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    match v.as_slice() {
        [x, y, w, h] => Some(Rect::new(*x, *y, *w, *h)),
        _ => None,
    }
}

fn print_monitors(monitors: &[Monitor]) {
    for m in monitors {
        println!(
            "{} {:?} work {:?} dpi {} ({:.0}%){}",
            m.device,
            m.rect,
            m.work,
            m.dpi,
            m.scale() * 100.0,
            if m.primary { " primary" } else { "" }
        );
    }
}

fn list_windows() {
    let targets = Targets::enumerate(screen::virtual_screen(), &[]);
    for t in &targets.list {
        let title: String = t.title.chars().take(70).collect();
        println!(
            "{}{:?} {} {:?}",
            "  ".repeat(t.depth as usize),
            t.rect,
            t.class,
            title
        );
    }
    println!("{} targets", targets.list.len());
}

fn grab(spec: &str, out: &Path) -> std::io::Result<()> {
    let layout = geometry::CaptureLayout::current()?;
    let (rect, layout) = if spec == "screen" {
        let monitor = layout.monitor_under_cursor()?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no display is available")
        })?;
        (monitor.rect, layout.with_monitor(&monitor)?)
    } else {
        let rect = parse_rect(spec).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "expected X,Y,W,H or 'screen'",
            )
        })?;
        (rect, layout)
    };
    let started = Instant::now();
    let frame = layout.grab(rect)?;
    let grabbed = started.elapsed();
    frame.save_png(out)?;
    let saved = started.elapsed() - grabbed;
    println!(
        "{rect:?} -> {} ({}x{}): grab {:.1} ms, PNG {:.1} ms, {} KiB",
        out.display(),
        frame.width,
        frame.height,
        ms(grabbed),
        ms(saved),
        std::fs::metadata(out)?.len() / 1024
    );
    Ok(())
}

struct Timing {
    samples: Vec<f64>,
}

impl Timing {
    fn new() -> Timing {
        Timing {
            samples: Vec::new(),
        }
    }

    fn time<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let started = Instant::now();
        let r = f();
        self.samples.push(ms(started.elapsed()));
        r
    }

    fn summary(&self) -> String {
        let mut s = self.samples.clone();
        s.sort_by(f64::total_cmp);
        let avg = s.iter().sum::<f64>() / s.len().max(1) as f64;
        let p95 = s
            .get((s.len() as f64 * 0.95) as usize)
            .or(s.last())
            .copied()
            .unwrap_or(0.0);
        format!(
            "{} updates, avg {avg:.2} ms, p95 {p95:.2} ms, max {:.2} ms",
            s.len(),
            s.last().copied().unwrap_or(0.0)
        )
    }
}

/// Prints what the overlay would show (the app draws it; the UI self-test
/// renders it).
fn print_view(overlay: &mut Overlay) -> std::io::Result<()> {
    let v = overlay.view()?;
    println!(
        "  shows {:?} ({:?}, {}) on {:?} at {}x",
        v.lit, v.title, v.size, v.monitor, v.scale
    );
    Ok(())
}

/// Builds the overlay exactly as the hotkey does, then feeds it scripted
/// input without showing a window, printing what it shows and timings.
fn selftest(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(target_os = "macos")]
    let display = screen::DisplayLayout::current()?;
    #[cfg(target_os = "macos")]
    let desktop = display.virtual_screen();
    #[cfg(windows)]
    let desktop = screen::virtual_screen();
    #[cfg(target_os = "macos")]
    let monitors = display.monitors().to_vec();
    #[cfg(windows)]
    let monitors = screen::monitors();
    println!("desktop {desktop:?}");
    print_monitors(&monitors);

    let started = Instant::now();
    #[cfg(target_os = "macos")]
    let frozen = display.freeze()?;
    #[cfg(target_os = "macos")]
    let snapshot = Frame::new(0, 0);
    #[cfg(windows)]
    let snapshot = screen::grab(desktop)?;
    let grabbed = started.elapsed();
    #[cfg(target_os = "macos")]
    let targets = Targets::enumerate_in_layout(desktop, &[], &display)?;
    #[cfg(windows)]
    let targets = Targets::enumerate(desktop, &[]);
    let enumerated = started.elapsed();
    let primary = monitors
        .iter()
        .find(|m| m.primary)
        .or(monitors.first())
        .expect("a monitor")
        .clone();
    let start = primary.rect.center();
    let overlay = Overlay::new(
        snapshot,
        (desktop.x, desktop.y),
        targets,
        monitors.clone(),
        start,
    )?;
    #[cfg(target_os = "macos")]
    let mut overlay = overlay
        .with_display_layout(geometry::CaptureLayout::from_display(display))
        .with_frozen(frozen, desktop);
    #[cfg(windows)]
    let mut overlay = overlay;
    println!(
        "overlay ready in {:.1} ms: grab {:.1} ms, {} targets in {:.1} ms, setup {:.1} ms",
        ms(started.elapsed()),
        ms(grabbed),
        overlay.targets.list.len(),
        ms(enumerated - grabbed),
        ms(started.elapsed() - enumerated)
    );
    let origin = overlay.origin;
    let client = |(x, y): (i32, i32)| (x - origin.0, y - origin.1);

    // Hover over the largest top-level window on the primary monitor.
    let window = overlay
        .targets
        .list
        .iter()
        .filter(|t| {
            t.depth == 0
                && primary
                    .rect
                    .intersect(t.rect)
                    .is_some_and(|r| r.area() * 2 > t.rect.area())
        })
        .max_by_key(|t| t.rect.area().min(primary.rect.area() * 9 / 10))
        .map(|t| t.rect);
    if let Some(rect) = window {
        let point = rect.inflate(-rect.width.min(rect.height) / 5).center();
        let (x, y) = client(point);
        overlay.pointer_moved(x, y);
        println!("hover {point:?}: {}", overlay.describe()?);
        print_view(&mut overlay)?;
        overlay.wheel(-120)?;
        overlay.wheel(120)?;
        println!("wheel up: {}", overlay.describe()?);
        overlay.wheel(-120)?;
        println!("wheel down: {}", overlay.describe()?);
    } else {
        println!("no window on the primary monitor to hover");
    }

    // Sweep the pointer across the primary monitor.
    let mut hover = Timing::new();
    let r = primary.rect;
    for i in 0..600 {
        let t = i as f64 / 599.0;
        let p = (
            r.x + (r.width as f64 * t) as i32,
            r.y + (r.height as f64 * (0.5 + 0.45 * (t * 12.0).sin())) as i32,
        );
        let (x, y) = client(p);
        hover.time(|| {
            overlay.pointer_moved(x, y);
            overlay.view()
        })?;
    }
    println!("hover sweep: {}", hover.summary());

    // Drag a region.
    let a = (r.x + r.width / 5, r.y + r.height / 5);
    let b = (r.x + r.width * 3 / 5, r.y + r.height * 3 / 5);
    let (ax, ay) = client(a);
    overlay.pointer_pressed(ax, ay);
    let mut drag = Timing::new();
    for i in 1..=120 {
        let x = a.0 + (b.0 - a.0) * i / 120;
        let y = a.1 + (b.1 - a.1) * i / 120;
        let (cx, cy) = client((x, y));
        drag.time(|| {
            overlay.pointer_moved(cx, cy);
            overlay.view()
        })?;
    }
    println!("drag: {}", drag.summary());
    print_view(&mut overlay)?;
    let (bx, by) = client(b);
    overlay.pointer_released(bx, by)?;
    let selection = overlay
        .outcome
        .clone()
        .flatten()
        .expect("drag produces a region");
    println!("selection: {selection:?}");

    let finish = Instant::now();
    let capture = workflow::from_overlay(&overlay, &selection, None, false, false)?
        .expect("selection inside desktop");
    let cropped = finish.elapsed();
    let out = dir.join("3-selection.png");
    capture.frame.save_png(&out)?;
    println!(
        "crop {:.1} ms, PNG {:.1} ms for {}x{} -> {}",
        ms(cropped),
        ms(finish.elapsed() - cropped),
        capture.frame.width,
        capture.frame.height,
        out.display()
    );

    // Keyboard: F picks the monitor under the cursor.
    overlay.outcome = None;
    overlay.key(b'F' as u16, false)?;
    println!("key F: {:?}", overlay.outcome.clone().flatten());

    // Freehand: a circle; outside it the capture is transparent.
    overlay.outcome = None;
    overlay.mode = overlay::Mode::Freehand;
    let (cx, cy) = client(r.center());
    let radius = r.height / 4;
    overlay.pointer_pressed(cx + radius, cy);
    for i in 1..=90 {
        let t = i as f64 / 90.0 * std::f64::consts::TAU;
        overlay.pointer_moved(
            cx + (radius as f64 * t.cos()) as i32,
            cy + (radius as f64 * t.sin()) as i32,
        );
    }
    overlay.pointer_released(cx + radius, cy)?;
    let freehand = overlay
        .outcome
        .clone()
        .flatten()
        .expect("freehand selection");
    let capture =
        workflow::from_overlay(&overlay, &freehand, None, false, false)?.expect("freehand capture");
    let px = |f: &Frame, x: u32, y: u32| f.pixels[(y * f.width + x) as usize * 4 + 3];
    let (w, h) = (capture.frame.width, capture.frame.height);
    println!(
        "freehand: {w}x{h}, corner alpha {}, center alpha {}",
        px(&capture.frame, 0, 0),
        px(&capture.frame, w / 2, h / 2)
    );
    let bounds = freehand.rect();
    assert!(
        bounds.width.abs_diff(2 * radius) <= 3 && bounds.height.abs_diff(2 * radius) <= 3,
        "freehand bounds follow the path"
    );
    assert!(
        px(&capture.frame, 0, 0) == 0 && px(&capture.frame, w / 2, h / 2) == 255,
        "freehand masks outside the path"
    );

    // Multiple areas: two drags, Backspace removes one, a third, Enter.
    overlay.outcome = None;
    overlay.mode = overlay::Mode::MultiArea;
    let drag = |o: &mut Overlay, a: (i32, i32), b: (i32, i32)| -> std::io::Result<()> {
        o.pointer_pressed(a.0, a.1);
        o.pointer_moved((a.0 + b.0) / 2, (a.1 + b.1) / 2);
        o.pointer_moved(b.0, b.1);
        o.pointer_released(b.0, b.1)
    };
    let (ox, oy) = client((r.x, r.y));
    drag(&mut overlay, (ox + 100, oy + 100), (ox + 300, oy + 200))?;
    drag(&mut overlay, (ox + 400, oy + 400), (ox + 500, oy + 450))?;
    overlay.key(sys::VK_BACK, false)?;
    drag(&mut overlay, (ox + 500, oy + 300), (ox + 600, oy + 380))?;
    overlay.key(sys::VK_RETURN, false)?;
    let multi = overlay
        .outcome
        .clone()
        .flatten()
        .expect("multi-area selection");
    let capture =
        workflow::from_overlay(&overlay, &multi, None, false, false)?.expect("multi capture");
    let (w, h) = (capture.frame.width, capture.frame.height);
    println!("multi: {multi:?} -> {w}x{h}");
    assert_eq!(
        (multi.rect().width, multi.rect().height),
        (501, 281),
        "two areas kept after Backspace (drags include the end pixel)"
    );
    assert!(
        px(&capture.frame, 1, 1) == 255 && px(&capture.frame, 1, h - 2) == 0,
        "gaps between areas are transparent"
    );

    // Fixed size: a click places the area around the pointer.
    overlay.outcome = None;
    overlay.mode = overlay::Mode::Fixed(640, 360);
    overlay.pointer_moved(cx, cy);
    overlay.pointer_pressed(cx, cy);
    overlay.pointer_released(cx, cy)?;
    let fixed = overlay.outcome.clone().flatten().expect("fixed selection");
    println!("fixed: {fixed:?}");
    #[cfg(windows)]
    assert_eq!((fixed.rect().width, fixed.rect().height), (640, 360));
    #[cfg(target_os = "macos")]
    assert!(matches!(
        fixed,
        overlay::Selection::Fixed {
            size: (640, 360),
            ..
        }
    ));

    // Capture effects run on the frame before it is saved.
    let mut frame = capture.frame.clone();
    workflow::apply_effects(&mut frame, &["Border".to_owned()]);
    println!(
        "effects: {}x{} -> {}x{} with a border",
        w, h, frame.width, frame.height
    );
    assert!(frame.width >= w && frame.height >= h);
    Ok(())
}

/// Runs the post-capture workflow (library save, editor hand-off) into
/// `dir`, with the clipboard left alone.
fn save_test(spec: &str, dir: &Path) -> std::io::Result<()> {
    let rect = parse_rect(spec)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected X,Y,W,H"))?;
    let capture = workflow::immediate(jiffshot_capture::library::CaptureKind::Region, rect, true)?;
    // The library goes where the test says (this command runs alone).
    std::env::set_var("JIFFSHOT_LIBRARY", dir);
    let settings = settings::Settings {
        copy_to_clipboard: false,
        ..settings::Settings::default()
    };
    let outcome = workflow::finish(capture, &settings, 0);
    println!(
        "{} (editor opened: {})",
        outcome.message, outcome.opened_editor
    );
    Ok(())
}

fn ocr(path: &Path) -> std::io::Result<()> {
    let frame = Frame::load_png(path)?;
    let started = Instant::now();
    let lines = jiffshot_capture::ocr::recognize(frame.width, frame.height, &frame.pixels)
        .map_err(std::io::Error::other)?;
    for line in &lines {
        println!("{:?} {}", line.rect, line.text);
    }
    println!("{} lines in {:.0} ms", lines.len(), ms(started.elapsed()));
    Ok(())
}

fn usage() {
    println!(
        "{}",
        include_str!("lib.rs")
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The capture a command-line flag asks for (`--window` opens the Capture
/// window); `None` for anything else.
pub fn capture_flag(args: &[String]) -> Option<usize> {
    Some(match args {
        [flag] => match flag.as_str() {
            "--capture" => tray::MODE_IMAGE,
            "--scrolling" => tray::MODE_SCROLLING,
            "--panoramic" => tray::MODE_PANORAMIC,
            "--text" => tray::MODE_TEXT,
            "--record" => tray::MODE_VIDEO,
            "--menu" => usize::MAX,
            "--steps" => tray::MODE_STEPS,
            "--window" => tray::OPEN_CAPTURE_WINDOW,
            "--freehand" => tray::MODE_FREEHAND,
            _ => return None,
        },
        _ => return None,
    })
}

/// Claims the one running Jiffshot for this session; false when another
/// process already has it (then [`forward_to_running`] hands it the request).
#[cfg(windows)]
pub fn claim_instance() -> bool {
    sys::claim_instance("Jiffshot.Capture")
}

#[cfg(target_os = "macos")]
pub fn claim_instance() -> std::io::Result<bool> {
    sys::claim_instance("Jiffshot.Capture")
}

/// Asks the running Jiffshot to start `mode` (a [`capture_flag`]), or to
/// open its Capture window when there is none.
#[cfg(windows)]
pub fn forward_to_running(mode: Option<usize>) {
    match mode {
        Some(usize::MAX) => sys::post_to_instance(tray::CLASS, tray::WM_APP_MENU, 0, 0),
        m => sys::post_to_instance(
            tray::CLASS,
            tray::WM_APP_CAPTURE,
            m.unwrap_or(tray::OPEN_CAPTURE_WINDOW),
            0,
        ),
    };
}

#[cfg(target_os = "macos")]
pub fn forward_to_running(mode: Option<usize>) -> std::io::Result<()> {
    let wait = Duration::from_secs(10);
    match mode {
        Some(usize::MAX) => {
            sys::post_to_instance_when_ready(tray::CLASS, tray::WM_APP_MENU, 0, 0, wait)
        }
        m => sys::post_to_instance_when_ready(
            tray::CLASS,
            tray::WM_APP_CAPTURE,
            m.unwrap_or(tray::OPEN_CAPTURE_WINDOW),
            0,
            wait,
        ),
    }
}

#[cfg(target_os = "macos")]
pub type EngineExit = Result<i32, String>;
#[cfg(not(target_os = "macos"))]
pub type EngineExit = i32;

/// Starts the capture side on its own thread. When it stops, `app.quit` runs
/// on that thread.
pub fn start_engine(
    initial: Option<usize>,
    app: Hooks,
) -> std::io::Result<std::thread::JoinHandle<EngineExit>> {
    #[cfg(target_os = "macos")]
    shutdown_macos::begin()?;
    let _ = HOOKS.set(app);
    // Scripts and agents drive this instance through `jiffshot-cli`.
    #[cfg(windows)]
    control::start();
    std::thread::Builder::new()
        .name("jiffshot-capture".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            let mut control = None;
            let code = tray::run(
                initial,
                #[cfg(target_os = "macos")]
                &mut control,
            );
            #[cfg(target_os = "macos")]
            let result = {
                let control = control
                    .map(|server| {
                        server
                            .shutdown()
                            .map_err(|error| format!("control shutdown failed: {error}"))
                    })
                    .transpose();
                let recording = control_macos::shutdown_recording();
                let capture = shutdown_macos::finish();
                let errors = [control.err(), recording.err(), capture.err()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if errors.is_empty() {
                    Ok(code)
                } else {
                    Err(errors.join("; "))
                }
            };
            if let Some(hooks) = hooks() {
                (hooks.quit)();
            }
            #[cfg(target_os = "macos")]
            return result;
            #[cfg(not(target_os = "macos"))]
            code
        })
}

/// Asks the capture side to stop (as its Exit does); safe from any thread.
pub fn stop_engine() {
    #[cfg(target_os = "macos")]
    shutdown_macos::request();
    sys::post_to_class(tray::CLASS, sys::WM_CLOSE, 0, 0);
}

#[cfg(target_os = "macos")]
pub fn start_recording_shutdown_selftest(
    rect: Rect,
) -> Result<std::sync::mpsc::Receiver<Result<Option<std::path::PathBuf>, String>>, String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none()
        || std::env::var_os("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_none()
    {
        return Err("the recording shutdown self-test is not enabled".into());
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    if !post_capture_thread(move || {
        let settings = settings::Settings {
            countdown: 0,
            record_audio: "none".into(),
            record_cursor: false,
            cursor_highlight: false,
            click_rings: false,
            webcam: None,
            ..Default::default()
        };
        let result = geometry::CaptureLayout::current()
            .map_err(|error| error.to_string())
            .and_then(|layout| recorder::record_in_layout(rect, &settings, layout));
        if let Err(error) = &result {
            shutdown_macos::work_failed(error);
        }
        if sender.send(result).is_err() {
            log("the recording shutdown self-test lost its result receiver");
        }
    }) {
        return Err("the capture engine is not ready for the recording shutdown self-test".into());
    }
    Ok(receiver)
}

#[cfg(target_os = "macos")]
pub fn start_image_shutdown_selftest() -> Result<
    (
        std::sync::mpsc::Receiver<Result<(), String>>,
        std::sync::mpsc::Sender<()>,
    ),
    String,
> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none()
        || std::env::var_os("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_none()
    {
        return Err("the image shutdown self-test is not enabled".into());
    }
    let (ready, received) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    if !post_capture_thread(move || tray::start_image_shutdown_selftest(ready, wait)) {
        return Err("the capture engine is not ready for the image shutdown self-test".into());
    }
    Ok((received, release))
}

#[cfg(target_os = "macos")]
pub fn image_shutdown_requested_for_selftest() -> Result<bool, String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none()
        || std::env::var_os("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_none()
    {
        return Err("the image shutdown self-test is not enabled".into());
    }
    Ok(shutdown_macos::requested())
}

#[cfg(target_os = "macos")]
pub fn finish_program_share_selftest(video: Option<&Path>) -> Result<String, String> {
    if std::env::var_os("JIFFSHOT_UITEST_SHARE_PROGRAM").is_none()
        && std::env::var_os("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_none()
    {
        return Err("the program sharing self-test is not enabled".into());
    }
    let root = std::env::var_os("JIFFSHOT_UITEST").ok_or("the self-test directory is missing")?;
    let root = Path::new(&root)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let library = std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the self-test library is missing")?;
    let library = Path::new(&library)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if library == root || !library.starts_with(&root) {
        return Err("the self-test library must be inside the test directory".into());
    }
    let settings = settings::Settings {
        copy_to_clipboard: false,
        open_in_editor: false,
        share: "program".into(),
        video_share: "program".into(),
        ..Default::default()
    };
    let outcome = if let Some(video) = video {
        let video = video.canonicalize().map_err(|error| error.to_string())?;
        if video == root || !video.starts_with(&root) {
            return Err("the self-test recording must be inside the test directory".into());
        }
        workflow::finish_video(&video, Rect::new(0, 0, 64, 48), &settings, 0)
    } else {
        let mut frame = Frame::new(8, 8);
        for pixel in frame.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[20, 40, 60, 255]);
        }
        let mut cursor = Frame::new(2, 2);
        cursor.alpha = true;
        for pixel in cursor.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[50, 100, 200, 255]);
        }
        let mut capture = workflow::from_frame(
            jiffshot_capture::library::CaptureKind::Region,
            frame.bounds(),
            frame,
        );
        capture.cursor = Some((cursor, (2, 3)));
        workflow::finish(capture, &settings, 0)
    };
    if outcome.is_error {
        Err(outcome.message)
    } else {
        Ok(outcome.message)
    }
}

#[cfg(target_os = "macos")]
pub fn start_program_share_shutdown_selftest(
) -> Result<std::sync::mpsc::Receiver<Result<Vec<String>, String>>, String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none()
        || std::env::var_os("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_none()
    {
        return Err("the file-panel shutdown self-test is not enabled".into());
    }
    let inject_failure = std::env::var("JIFFSHOT_UITEST_PANEL_SHUTDOWN").as_deref() == Ok("retry");
    let (sender, receiver) = std::sync::mpsc::channel();
    if !post_capture_thread(move || {
        shutdown_macos::spawn(move || {
            let result = finish_program_share_selftest(None).and_then(|first| {
                finish_program_share_selftest(None).map(|second| vec![first, second])
            });
            let failure = result.as_ref().err().cloned().or_else(|| {
                inject_failure.then(|| "injected file-panel shutdown failure".to_owned())
            });
            if sender.send(result).is_err() {
                crate::log("the file-panel shutdown self-test stopped waiting for its result");
            }
            failure
        });
    }) {
        return Err("the capture engine is not ready for the file-panel shutdown self-test".into());
    }
    Ok(receiver)
}

#[cfg(target_os = "macos")]
pub fn recording_frames_for_selftest() -> Result<Option<u64>, String> {
    hud::recording_frames_for_selftest()
}

/// Starts a capture (by [`capture_flag`] mode) from the editor after
/// `delay_ms`, so its window can get out of the way first.
pub fn request_capture(mode: usize, delay_ms: u32) {
    sys::post_to_class(
        tray::CLASS,
        tray::WM_APP_CAPTURE_LATER,
        mode,
        delay_ms as isize,
    );
}

/// Runs a command-line mode that does its work and exits (`--grab`,
/// `--selftest` and the rest), returning its exit code; `None` when `args`
/// ask for the app itself (nothing, a capture flag, or files to open).
pub fn run_command(args: &[String]) -> Option<i32> {
    if args.is_empty() || capture_flag(args).is_some() || !args[0].starts_with("--") {
        return None;
    }
    #[cfg(windows)]
    // SAFETY: attaches to the launching console so CLI output is visible
    // from a GUI-subsystem build; harmless when there is none.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        )
    };
    screen::enable_per_monitor_dpi();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--write-icon", out] => icon::ico().and_then(|bytes| std::fs::write(out, bytes)),
        ["--write-logo", size, out] => size
            .parse::<u32>()
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "logo size must be a positive whole number",
                )
            })
            .and_then(icon::png)
            .and_then(|bytes| std::fs::write(out, bytes)),
        ["--write-logo", ..] => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: --write-logo SIZE OUT.png",
        )),
        ["--monitors"] => {
            print_monitors(&screen::monitors());
            Ok(())
        }
        ["--list-windows"] => {
            list_windows();
            Ok(())
        }
        ["--grab", spec, out] => grab(spec, Path::new(out)),
        ["--quit"] => {
            #[cfg(windows)]
            {
                sys::post_to_instance(tray::CLASS, sys::WM_CLOSE, 0, 0);
                Ok(())
            }
            #[cfg(target_os = "macos")]
            {
                sys::post_to_instance_when_ready(
                    tray::CLASS,
                    tray::WM_APP_REQUEST_EXIT,
                    0,
                    0,
                    Duration::ZERO,
                )
            }
        }
        ["--save-test", spec, dir] => save_test(spec, Path::new(dir)),
        ["--ocr", file] => ocr(Path::new(file)),
        ["--scroll-selftest", dir] => selftest_scroll::run(Path::new(dir)),
        ["--menu-selftest", dir] => selftest_scroll::menus(Path::new(dir)),
        #[cfg(target_os = "macos")]
        ["--internal-menu-fixture"] => selftest_scroll::menu_fixture(),
        #[cfg(target_os = "macos")]
        ["--hotkey-selftest"] => selftest_scroll::hotkeys(),
        #[cfg(target_os = "macos")]
        ["--step-selftest", dir] => stepcap::selftest(Path::new(dir)),
        #[cfg(target_os = "macos")]
        ["--internal-step-fixture"] => stepcap::selftest_fixture(),
        ["--record-selftest", dir, seconds] => {
            selftest_scroll::record(Path::new(dir), seconds.parse().unwrap_or(3.0))
        }
        ["--selftest", dir] => selftest(Path::new(dir)),
        _ => {
            usage();
            Ok(())
        }
    };
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use jiffshot_capture::wide;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, GetDesktopWindow, HWND_MESSAGE};

    #[test]
    fn capture_dispatch_accepts_only_windows_in_this_process() {
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                // SAFETY: this test owns the window and runs on its creating thread.
                unsafe { DestroyWindow(self.0) };
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
        assert!(is_local_window(window.0));
        assert!(!is_local_window(std::ptr::null_mut()));
        // SAFETY: obtaining the system desktop window is read-only.
        assert!(!is_local_window(unsafe { GetDesktopWindow() }));
    }
}
